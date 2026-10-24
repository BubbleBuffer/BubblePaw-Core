//! Tool-call approvals (docs/approvals.md). One gate serves direct model calls
//! and nested host calls. It runs after validation and before the effect
//! boundary, so a waiting or denied call never leaves a dispatched effect.
//! The store record is the truth; the in-memory waiter only wakes the gate.
use crate::{EventKind, Harness, Store, ToolCall, ToolContext, id, now};
use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::oneshot;

mod blocklist;
mod modes;
mod rules;

use blocklist::hardline_message;
pub(crate) use modes::AutoApprovalContext;
use modes::auto_reason_logged;
pub use modes::{Approval, ApprovalMode, ApprovalPolicy, AutoApprovalConfig, DEFAULT_AUTO_PROMPT};
use rules::has_rule;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    Pending,
    Approved,
    ApprovedSession,
    Denied,
    Expired,
    Cancelled,
}
impl ApprovalState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::ApprovedSession => "approved_session",
            Self::Denied => "denied",
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
        }
    }
    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "pending" => Self::Pending,
            "approved" => Self::Approved,
            "approved_session" => Self::ApprovedSession,
            "denied" => Self::Denied,
            "expired" => Self::Expired,
            "cancelled" => Self::Cancelled,
            _ => bail!("Unknown approval state: {value}"),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub id: String,
    pub session_id: String,
    pub run_id: String,
    pub call_id: String,
    pub tool: String,
    pub arguments: Value,
    pub state: ApprovalState,
    pub reason: Option<String>,
    pub created_at: String,
    pub decided_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Approve,
    ApproveForSession,
    Deny { reason: Option<String> },
}
impl Decision {
    /// The wire form shared by the runner surfaces: `approve`, `approve_session`
    /// or `deny` with an optional reason.
    pub fn parse(decision: &str, reason: Option<String>) -> Result<Self> {
        Ok(match decision {
            "approve" => Self::Approve,
            "approve_session" => Self::ApproveForSession,
            "deny" => Self::Deny { reason },
            _ => bail!("decision must be approve, approve_session or deny"),
        })
    }
}

/// Wakes gates waiting on a decision. Shared by every harness on one store.
#[derive(Default)]
pub(crate) struct Waiters(Mutex<HashMap<String, oneshot::Sender<ApprovalRecord>>>);
/// A registered wait. Dropped unsettled (cancelled run, aborted cell), it
/// records the approval as cancelled, so no pending record outlives its gate.
struct Pending<'a> {
    store: &'a Store,
    runtime: Arc<crate::runtime::RuntimeState>,
    id: String,
    settled: bool,
}
impl<'a> Pending<'a> {
    fn register(store: &'a Store, id: &str, sender: oneshot::Sender<ApprovalRecord>) -> Self {
        let runtime = store.runtime();
        runtime
            .approvals
            .0
            .lock()
            .unwrap()
            .insert(id.into(), sender);
        Self {
            store,
            runtime,
            id: id.into(),
            settled: false,
        }
    }
}
impl Drop for Pending<'_> {
    fn drop(&mut self) {
        self.runtime.approvals.0.lock().unwrap().remove(&self.id);
        if !self.settled {
            let _ = transition(self.store, &self.id, ApprovalState::Cancelled, None);
        }
    }
}

tokio::task_local! {
    static WAITS: Arc<AtomicUsize>;
}
/// Count approval waits of gates reached inside `future`. Hosts with idle or
/// call deadlines use it to exclude time spent waiting for a person.
pub async fn track_approval_waits<F: Future>(counter: Arc<AtomicUsize>, future: F) -> F::Output {
    WAITS.scope(counter, future).await
}
struct Waiting(Option<Arc<AtomicUsize>>);
impl Waiting {
    fn start() -> Self {
        Self(
            WAITS
                .try_with(|counter| {
                    counter.fetch_add(1, Ordering::AcqRel);
                    counter.clone()
                })
                .ok(),
        )
    }
}
impl Drop for Waiting {
    fn drop(&mut self) {
        if let Some(counter) = &self.0 {
            counter.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

pub(crate) enum Gate {
    Proceed,
    Denied(String),
    Cancelled,
}
impl Gate {
    /// None to proceed, the tool error for a denial, or the call site's
    /// existing cancellation error.
    pub(crate) fn denial(self, cancelled: &str) -> Result<Option<String>> {
        match self {
            Self::Proceed => Ok(None),
            Self::Denied(message) => Ok(Some(message)),
            Self::Cancelled => bail!("{cancelled}"),
        }
    }
}

fn denial(reason: Option<&str>) -> String {
    let reason = match reason.map(str::trim).filter(|r| !r.is_empty()) {
        Some(reason) if reason.ends_with(['.', '!', '?']) => format!(": {reason}"),
        Some(reason) => format!(": {reason}."),
        None => ".".into(),
    };
    format!("Denied by the user{reason} Do not retry this call unless the user asks.")
}

fn resolved(record: &ApprovalRecord) -> EventKind {
    EventKind::ApprovalResolved {
        approval_id: record.id.clone(),
        state: record.state,
        reason: record.reason.clone(),
    }
}
fn resolved_event(record: &ApprovalRecord) -> crate::event_log::JournalEvent {
    (
        resolved(record),
        Some(crate::event_log::ContentRef::ApprovalResolution {
            approval_id: record.id.clone(),
        }),
    )
}

/// Wait for the effective decision on one validated call.
pub(crate) async fn gate(
    store: &Store,
    policy: &ApprovalPolicy,
    auto: Option<&AutoApprovalContext>,
    context: &ToolContext,
    workspaces: &[std::path::PathBuf],
    source: CallSource<'_>,
    definition: &crate::ToolDefinition,
) -> Result<Gate> {
    let CallSource {
        call,
        message_id,
        interactive,
    } = source;
    // The hardline floor precedes configured denies, allowances and every mode.
    let hardline = policy.hardline_match(&call.name, &call.arguments, workspaces);
    let denied_rule = policy.denied_rule(&call.name);
    let mode = policy.mode(&call.name, definition.approval);
    if hardline.is_none()
        && denied_rule.is_none()
        && (mode == ApprovalMode::Allow || has_rule(store, &context.session_id, &call.name)?)
    {
        return Ok(Gate::Proceed);
    }
    let mut record = ApprovalRecord {
        id: id(),
        session_id: context.session_id.clone(),
        run_id: context.run_id.clone(),
        call_id: call.id.clone(),
        tool: call.name.clone(),
        arguments: call.arguments.clone(),
        state: ApprovalState::Pending,
        reason: None,
        created_at: now(),
        decided_at: None,
    };
    if hardline.is_some() || denied_rule.is_some() || mode == ApprovalMode::Deny {
        record.state = ApprovalState::Denied;
        record.reason = Some(match (&hardline, denied_rule) {
            (Some(pattern), _) => format!("hardline {pattern}"),
            (None, Some(rule)) => format!("deny rule {rule}"),
            (None, None) => "policy".into(),
        });
        record.decided_at = Some(now());
        insert_with_source(store, &record, message_id)?;
        return Ok(Gate::Denied(match &hardline {
            Some(pattern) => hardline_message(pattern),
            None => denial(record.reason.as_deref()),
        }));
    }
    if !interactive {
        let reason = "Interactive approval is unavailable for model-free tool calls; authorize the tool with an allow policy before calling it";
        record.state = ApprovalState::Denied;
        record.reason = Some(reason.into());
        record.decided_at = Some(now());
        insert_with_source(store, &record, message_id)?;
        return Ok(Gate::Denied(reason.into()));
    }
    if mode == ApprovalMode::Auto
        && let Some(config) = auto
        && let Some(reason) = auto_reason_logged(store, config, context, call, definition).await
    {
        record.state = ApprovalState::Approved;
        record.reason = Some(reason);
        record.decided_at = Some(now());
        insert_with_source(store, &record, message_id)?;
        return Ok(Gate::Proceed);
    }
    let (sender, receiver) = oneshot::channel();
    // Register before the record is visible, so every decision finds its waiter.
    let mut pending = Pending::register(store, &record.id, sender);
    insert_with_source(store, &record, message_id)?;
    let _waiting = Waiting::start();
    let timeout = async {
        match policy.timeout_secs {
            Some(seconds) => tokio::time::sleep(Duration::from_secs(seconds)).await,
            None => std::future::pending().await,
        }
    };
    let decided = tokio::select! {
        biased;
        // Dropping the guard records the cancellation.
        _ = context.cancellation.cancelled() => return Ok(Gate::Cancelled),
        decided = receiver => match decided {
            Ok(record) => record,
            Err(_) => load(store, &record.id)?,
        },
        _ = timeout => match transition(store, &record.id, ApprovalState::Expired, Some("timeout"))? {
            Some(record) => record,
            None => load(store, &record.id)?,
        },
    };
    pending.settled = true;
    Ok(match decided.state {
        ApprovalState::Approved | ApprovalState::ApprovedSession => Gate::Proceed,
        ApprovalState::Denied => Gate::Denied(denial(decided.reason.as_deref())),
        ApprovalState::Expired => Gate::Denied("Approval timed out.".into()),
        ApprovalState::Cancelled => Gate::Cancelled,
        ApprovalState::Pending => bail!("Approval {} is still pending", decided.id),
    })
}

pub(crate) struct CallSource<'a> {
    pub(crate) call: &'a ToolCall,
    pub(crate) message_id: Option<&'a str>,
    pub(crate) interactive: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum ArgumentsRef {
    Message { message_id: String, call_id: String },
    Blob { hash: String },
}

/// Record a decision and wake its gate. Unknown or decided approvals are errors.
#[cfg(test)]
fn decide(store: &Store, approval_id: &str, decision: Decision) -> Result<ApprovalRecord> {
    decide_record(store, approval_id, decision, None)
}
fn decide_record(
    store: &Store,
    approval_id: &str,
    decision: Decision,
    client: Option<crate::ClientInfo>,
) -> Result<ApprovalRecord> {
    let waiter = store
        .runtime()
        .approvals
        .0
        .lock()
        .unwrap()
        .remove(approval_id);
    let (state, reason) = match decision {
        Decision::Approve => (ApprovalState::Approved, None),
        Decision::ApproveForSession => (ApprovalState::ApprovedSession, None),
        Decision::Deny { reason } => (ApprovalState::Denied, reason),
    };
    // On failure the waiter is dropped; its gate then reads the stored state.
    let identity = load(store, approval_id)?;
    if identity.state != ApprovalState::Pending {
        bail!(
            "Approval {approval_id} is already {}",
            identity.state.as_str()
        );
    }
    let record = store.runtime().log.commit(store, &identity.session_id, &identity.run_id, client, |tx| {
        let current: Option<String> = tx
            .query_row(
                "SELECT state FROM approvals WHERE id=?1",
                [approval_id],
                |r| r.get(0),
            )
            .optional()?;
        // Another decision can resolve/archive the row between the identity
        // read and this serialized commit. Its terminal state lives in serving.
        let current = match current {
            Some(current) => Some(current),
            None => store.archived_approval_state(approval_id)?,
        };
        match current.as_deref() {
            None => bail!("Unknown approval: {approval_id}"),
            Some("pending") => {}
            Some(state) => bail!("Approval {approval_id} is already {state}"),
        }
        tx.execute(
            "UPDATE approvals SET state=?2,reason=?3,decided_at=?4 WHERE id=?1 AND state='pending'",
            params![approval_id, state.as_str(), reason, now()],
        )?;
        if state == ApprovalState::ApprovedSession {
            tx.execute(
                "INSERT OR IGNORE INTO approval_rules(session_id,tool) SELECT session_id,tool FROM approvals WHERE id=?1",
                [approval_id],
            )?;
        }
        let record = read(tx, approval_id)?;
        let event = resolved_event(&record);
        Ok((record, vec![event]))
    })?;
    if let Some(waiter) = waiter {
        let _ = waiter.send(record.clone());
    }
    Ok(record)
}

/// Pending approvals of a session, oldest first.
pub(crate) fn pending(store: &Store, session_id: &str) -> Result<Vec<ApprovalRecord>> {
    if let Some(chat) = store.scoped(session_id)? {
        return pending(&chat, session_id);
    }
    store.read(|conn| {
        let mut statement = conn.prepare(
            "SELECT id FROM approvals WHERE session_id=?1 AND state='pending' ORDER BY rowid",
        )?;
        let ids = statement
            .query_map([session_id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.iter().map(|id| read(conn, id)).collect()
    })
}

/// No gate survives a restart: pending approvals expire with their runs.
pub(crate) fn recover(store: &Store) -> Result<()> {
    let pending: bool = store.read(|conn| {
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM approvals WHERE state='pending')",
            [],
            |r| r.get(0),
        )?)
    })?;
    if pending {
        store.write(|tx| {
            tx.execute(
                "UPDATE approvals SET state='expired',reason='restart',decided_at=?1 WHERE state='pending'",
                [now()],
            )?;
            Ok(())
        })?;
    }
    Ok(())
}

pub(crate) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS approvals(id TEXT PRIMARY KEY,session_id TEXT NOT NULL REFERENCES sessions(id),run_id TEXT NOT NULL,call_id TEXT NOT NULL,tool TEXT NOT NULL,arguments TEXT NOT NULL,state TEXT NOT NULL,reason TEXT,created_at TEXT NOT NULL,decided_at TEXT); CREATE INDEX IF NOT EXISTS approvals_session ON approvals(session_id,state); CREATE TABLE IF NOT EXISTS approval_rules(session_id TEXT NOT NULL REFERENCES sessions(id),tool TEXT NOT NULL,PRIMARY KEY(session_id,tool));";

#[cfg(test)]
fn insert(store: &Store, record: &ApprovalRecord) -> Result<()> {
    insert_with_source(store, record, None)
}
fn insert_with_source(
    store: &Store,
    record: &ApprovalRecord,
    message_id: Option<&str>,
) -> Result<()> {
    store.runtime().log.commit(store, &record.session_id, &record.run_id, None, |tx| {
        let reference = match message_id {
            Some(message_id) => ArgumentsRef::Message { message_id: message_id.into(), call_id: record.call_id.clone() },
            None => ArgumentsRef::Blob { hash: Store::blob_put(tx, &serde_json::to_vec(&record.arguments)?)? },
        };
        tx.execute(
            "INSERT INTO approvals(id,session_id,run_id,call_id,tool,arguments,state,reason,created_at,decided_at,arguments_ref) VALUES(?1,?2,?3,?4,?5,'',?6,?7,?8,?9,?10)",
            params![
                record.id,
                record.session_id,
                record.run_id,
                record.call_id,
                record.tool,
                record.state.as_str(),
                record.reason,
                record.created_at,
                record.decided_at,
                serde_json::to_string(&reference)?
            ],
        )?;
        let mut events = vec![(EventKind::ApprovalRequested { approval: record.clone() },
            Some(crate::event_log::ContentRef::Approval { approval_id: record.id.clone() }))];
        if record.state != ApprovalState::Pending { events.push(resolved_event(record)); }
        Ok(((), events))
    })
}
fn transition(
    store: &Store,
    approval_id: &str,
    state: ApprovalState,
    reason: Option<&str>,
) -> Result<Option<ApprovalRecord>> {
    let identity = load(store, approval_id)?;
    store
        .runtime()
        .log
        .commit(store, &identity.session_id, &identity.run_id, None, |tx| {
            let changed = tx.execute(
            "UPDATE approvals SET state=?2,reason=?3,decided_at=?4 WHERE id=?1 AND state='pending'",
            params![approval_id, state.as_str(), reason, now()],
        )?;
            if changed == 0 {
                return Ok((None, Vec::new()));
            }
            let record = read(tx, approval_id)?;
            let event = resolved_event(&record);
            Ok((Some(record), vec![event]))
        })
}
fn load(store: &Store, approval_id: &str) -> Result<ApprovalRecord> {
    store.approval_record(approval_id)
}
fn read(conn: &Connection, approval_id: &str) -> Result<ApprovalRecord> {
    read_record(conn, approval_id)
}
pub(crate) fn read_record(conn: &Connection, approval_id: &str) -> Result<ApprovalRecord> {
    read_record_from(conn, conn, approval_id)
}
pub(crate) fn read_record_from(
    conn: &Connection,
    records: &Connection,
    approval_id: &str,
) -> Result<ApprovalRecord> {
    let row = records
        .query_row(
            "SELECT id,session_id,run_id,call_id,tool,arguments,state,reason,created_at,decided_at,arguments_ref FROM approvals WHERE id=?1",
            [approval_id],
            |r| {
                Ok((
                    (
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ),
                    (
                        r.get::<_, String>(5)?,
                        r.get::<_, String>(6)?,
                        r.get::<_, Option<String>>(7)?,
                        r.get::<_, String>(8)?,
                        r.get::<_, Option<String>>(9)?,
                        r.get::<_, Option<String>>(10)?,
                    ),
                ))
            },
        )
        .optional()?;
    let Some((
        (id, session_id, run_id, call_id, tool),
        (arguments, state, reason, created_at, decided_at, arguments_ref),
    )) = row
    else {
        bail!("Unknown approval: {approval_id}");
    };
    let arguments = match arguments_ref
        .map(|reference| serde_json::from_str::<ArgumentsRef>(&reference))
        .transpose()?
    {
        Some(ArgumentsRef::Message {
            message_id,
            call_id,
        }) => {
            Store::read_message(conn, &session_id, &message_id)?
                .tool_calls
                .into_iter()
                .find(|call| call.id == call_id)
                .ok_or_else(|| anyhow::anyhow!("Approval tool call missing"))?
                .arguments
        }
        Some(ArgumentsRef::Blob { hash }) => {
            serde_json::from_slice(&Store::blob_get(conn, &hash)?)?
        }
        None => serde_json::from_str(&arguments)?,
    };
    Ok(ApprovalRecord {
        id,
        session_id,
        run_id,
        call_id,
        tool,
        arguments,
        state: ApprovalState::parse(&state)?,
        reason,
        created_at,
        decided_at,
    })
}

impl Harness {
    /// Approvals waiting for a person in this session, oldest first.
    pub fn pending_approvals(&self, session_id: &str) -> Result<Vec<ApprovalRecord>> {
        pending(self.store(), session_id)
    }
    /// Decide a pending approval. Deciding twice, or an unknown id, is an error.
    pub fn decide(&self, approval_id: &str, decision: Decision) -> Result<ApprovalRecord> {
        self.decide_from(approval_id, decision, None)
    }
    /// Decide, recording the client on ApprovalResolved (docs/session-api.md).
    pub fn decide_from(
        &self,
        approval_id: &str,
        decision: Decision,
        client: Option<crate::ClientInfo>,
    ) -> Result<ApprovalRecord> {
        decide_record(self.store(), approval_id, decision, client)
    }
}

#[cfg(test)]
#[path = "../approvals_tests.rs"]
mod tests;
