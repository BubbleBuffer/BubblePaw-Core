//! The multi-client session API (docs/session-api.md). Every harness on one
//! store, parent and child agents alike, emits into one sequence and one
//! broadcast. Journal references enter the serving buffer before broadcast,
//! under one lock, so broadcast order is sequence order. Canonical state and
//! external effect boundaries commit synchronously; directory-store journal
//! flushes are disposable and occur at idle.
//! `Paragraph` is live-only: it carries the sequence of the last durable event
//! before it, and its text is kept in memory as the run's streaming text until
//! the assistant message that contains it is stored.
use crate::{EventKind, Harness, HarnessEvent, Store, now};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
    sync::{Mutex, OnceLock},
};
use tokio::sync::broadcast;

/// The most events one `events_after` call returns.
pub const EVENTS_AFTER_LIMIT: usize = 1000;
/// Streaming text kept per session; text beyond it is live-only.
const STREAMING_LIMIT: usize = 1024 * 1024;
const PRUNED_KEY: &str = "events_pruned_through";
/// The last sequence ever assigned, kept when deleting a session removes the newest events.
const LAST_KEY: &str = "events_last_seq";

/// The client that sent a command; carried as `origin` by the event it causes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientInfo {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

pub(crate) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS events(seq INTEGER PRIMARY KEY,session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,run_id TEXT NOT NULL,created_at TEXT NOT NULL,kind TEXT NOT NULL,data TEXT NOT NULL); CREATE INDEX IF NOT EXISTS events_session ON events(session_id,seq);";

/// The sequence, broadcast and streaming text shared by every harness on a store.
#[derive(Default)]
pub(crate) struct Log {
    state: Mutex<State>,
    pruned_floor: AtomicU64,
    sender: OnceLock<broadcast::Sender<HarnessEvent>>,
}
#[derive(Default)]
struct State {
    last: u64,
    pruned_through: u64,
    streaming: HashMap<String, Streaming>,
    /// Live events per session since the last durable event.
    live: HashMap<String, u64>,
}
struct Streaming {
    run_id: String,
    text: String,
}

/// Only the persisted journal uses these references. Subscribers and transports
/// continue to receive the complete, unchanged `EventKind` wire payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub(crate) enum ContentRef {
    ToolCall { message_id: String, call_id: String },
    ToolResult { message_id: String },
    Input { input_id: String },
    Output { output_id: String },
    Approval { approval_id: String },
    ApprovalResolution { approval_id: String },
    Run { run_id: String },
    Blob { hash: String },
}
pub(crate) type JournalEvent = (EventKind, Option<ContentRef>);
type JournalBatch<T> = (T, Vec<JournalEvent>);

#[path = "event_log_commit.rs"]
mod commit;
pub(crate) use commit::{PendingEvent, parent_run_state_event};

/// What a snapshot read under [`Harness::snapshot_with`] reflects.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct LogPosition {
    /// The last durable sequence the snapshot reflects. Follow with events after it.
    pub seq: u64,
    /// The running run's text since its last stored message, paragraphs joined by a blank line.
    pub streaming_text: String,
    /// Live events of this session already broadcast with sequence `seq`, so
    /// already in `streaming_text`. A follower skips that many live events
    /// carrying `seq` and applies the rest.
    pub live_seen: u64,
}

impl Log {
    pub(crate) fn reset_serving(&self, replace: impl FnOnce(u64) -> Result<()>) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        replace(state.last)?;
        state.pruned_through = state.last;
        self.pruned_floor.store(state.last, Ordering::Relaxed);
        Ok(())
    }
    /// Reconciliation may run while a caller already holds the journal lock.
    /// Publish its gap without recursively taking that lock.
    pub(crate) fn mark_pruned(&self, through: u64) {
        self.pruned_floor.fetch_max(through, Ordering::Relaxed);
    }
    /// Called at store open with the log's current bounds.
    pub(crate) fn seed(&self, last: u64, pruned_through: u64) {
        let mut state = self.state.lock().unwrap();
        state.last = last;
        state.pruned_through = pruned_through;
        self.pruned_floor.store(pruned_through, Ordering::Relaxed);
    }
    /// The last durable sequence assigned.
    pub(crate) fn last(&self) -> u64 {
        self.state.lock().unwrap().last
    }
    /// Drop a deleted session's streaming text and live counts.
    pub(crate) fn forget_session(&self, session_id: &str) {
        let mut state = self.state.lock().unwrap();
        state.streaming.remove(session_id);
        state.live.remove(session_id);
    }
    /// The broadcast is created by the first harness on the store, with its capacity.
    pub(crate) fn init(&self, capacity: usize) {
        self.sender
            .get_or_init(|| broadcast::channel(capacity.max(1)).0);
    }
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<HarnessEvent> {
        self.sender
            .get_or_init(|| broadcast::channel(256).0)
            .subscribe()
    }
    /// Persist a reference and broadcast one full event; text progress stays
    /// live. A failed write leaves the event live-only without a sequence hole.
    pub(crate) fn emit(
        &self,
        store: &Store,
        session_id: &str,
        run_id: &str,
        kind: EventKind,
        origin: Option<ClientInfo>,
    ) {
        if matches!(
            kind,
            EventKind::Paragraph { .. } | EventKind::TextDelta { .. }
        ) {
            self.transient(session_id, run_id, kind);
            return;
        }
        // Legacy fire-and-forget callers remain live on failure; transactional
        // runtime callers use `commit` and propagate the write error instead.
        let fallback = kind.clone();
        if self
            .commit(store, session_id, run_id, origin.clone(), |_| {
                Ok(((), vec![(kind, None)]))
            })
            .is_err()
        {
            self.transient_from(session_id, run_id, fallback, origin);
        }
    }
    /// Presentation of nested calls is progress, not another copy of host
    /// arguments and results in the durable journal.
    pub(crate) fn transient(&self, session_id: &str, run_id: &str, kind: EventKind) {
        self.transient_from(session_id, run_id, kind, None);
    }
    fn transient_from(
        &self,
        session_id: &str,
        run_id: &str,
        kind: EventKind,
        origin: Option<ClientInfo>,
    ) {
        let mut state = self.state.lock().unwrap();
        if let EventKind::Paragraph { text } = &kind {
            state.stream(session_id, run_id, text);
        }
        if let Some(sender) = self.sender.get() {
            let _ = sender.send(HarnessEvent {
                sequence: state.last,
                durable: false,
                origin,
                session_id: session_id.into(),
                run_id: run_id.into(),
                created_at: now(),
                kind,
            });
        }
    }
    /// Broadcast a live-only paragraph. Used by provider callbacks, which must
    /// not retain the store.
    pub(crate) fn paragraph(&self, session_id: &str, run_id: &str, text: String) {
        let mut state = self.state.lock().unwrap();
        state.stream(session_id, run_id, &text);
        let event = HarnessEvent {
            sequence: state.last,
            durable: false,
            origin: None,
            session_id: session_id.into(),
            run_id: run_id.into(),
            created_at: now(),
            kind: EventKind::Paragraph { text },
        };
        if let Some(sender) = self.sender.get() {
            let _ = sender.send(event);
        }
    }
    /// Broadcast one streamed text batch as live-only progress. It is neither
    /// stored nor added to `streaming_text`, and it does not count toward
    /// `live_seen`, which counts paragraphs only.
    pub(crate) fn progress(&self, session_id: &str, run_id: &str, text: String) {
        let state = self.state.lock().unwrap();
        let event = HarnessEvent {
            sequence: state.last,
            durable: false,
            origin: None,
            session_id: session_id.into(),
            run_id: run_id.into(),
            created_at: now(),
            kind: EventKind::TextDelta { text },
        };
        if let Some(sender) = self.sender.get() {
            let _ = sender.send(event);
        }
    }
    /// Store a message that contains the streamed text, and clear that text,
    /// atomically with respect to snapshots.
    pub(crate) fn settle_text<T>(
        &self,
        session_id: &str,
        store: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let mut state = self.state.lock().unwrap();
        let value = store()?;
        if let Some(streaming) = state.streaming.get_mut(session_id) {
            streaming.text.clear();
        }
        Ok(value)
    }
}
impl State {
    fn stream(&mut self, session_id: &str, run_id: &str, text: &str) {
        *self.live.entry(session_id.into()).or_default() += 1;
        let streaming = self
            .streaming
            .entry(session_id.into())
            .or_insert_with(|| Streaming {
                run_id: run_id.into(),
                text: String::new(),
            });
        if streaming.run_id != run_id {
            streaming.run_id = run_id.into();
            streaming.text.clear();
        }
        let separator = if streaming.text.is_empty() {
            ""
        } else {
            "\n\n"
        };
        if streaming.text.len() + separator.len() + text.len() <= STREAMING_LIMIT {
            streaming.text.push_str(separator);
            streaming.text.push_str(text);
        }
    }
}

fn kind_name(kind: &EventKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value["type"].as_str().map(str::to_owned))
        .unwrap_or_default()
}
fn encode(tx: &Connection, event: &HarnessEvent, reference: Option<ContentRef>) -> Result<String> {
    let mut value = serde_json::to_value(event)?;
    let reference = match reference {
        Some(reference) => Some(reference),
        None => match &event.kind {
            EventKind::Output { record } => Some(ContentRef::Output {
                output_id: record.id.clone(),
            }),
            EventKind::ApprovalRequested { approval } => Some(ContentRef::Approval {
                approval_id: approval.id.clone(),
            }),
            EventKind::RunFinished { .. } => Some(ContentRef::Run {
                run_id: event.run_id.clone(),
            }),
            EventKind::ToolStarted { .. }
            | EventKind::ToolFinished { .. }
            | EventKind::SteeringAccepted { .. }
            | EventKind::FollowupQueued { .. } => {
                // Public test/host emitters may have no owning row. Keep their
                // contents once in tier 1 rather than embedding them in events.
                Some(ContentRef::Blob {
                    hash: Store::blob_put(tx, &serde_json::to_vec(&event.kind)?)?,
                })
            }
            _ => None,
        },
    };
    if let Some(reference) = &reference {
        match reference {
            ContentRef::ToolCall { .. } => {
                value.as_object_mut().unwrap().remove("call");
            }
            ContentRef::ToolResult { .. } => {
                value["result"].as_object_mut().unwrap().remove("content");
            }
            ContentRef::Input { .. } => {
                value.as_object_mut().unwrap().remove("text");
            }
            ContentRef::Output { .. } => {
                value["record"].as_object_mut().unwrap().remove("output");
            }
            ContentRef::Approval { .. } => {
                value["approval"]
                    .as_object_mut()
                    .unwrap()
                    .remove("arguments");
                value["approval"].as_object_mut().unwrap().remove("reason");
            }
            ContentRef::ApprovalResolution { .. } => {
                value.as_object_mut().unwrap().remove("reason");
            }
            ContentRef::Run { .. } => {
                value.as_object_mut().unwrap().remove("text");
                value.as_object_mut().unwrap().remove("error");
                value.as_object_mut().unwrap().remove("usage");
            }
            ContentRef::Blob { .. } => {
                let kind = serde_json::to_value(&event.kind)?;
                for key in kind
                    .as_object()
                    .unwrap()
                    .keys()
                    .filter(|key| key.as_str() != "type")
                {
                    value.as_object_mut().unwrap().remove(key);
                }
            }
        }
        value["_content_ref"] = serde_json::to_value(reference)?;
    }
    let data = serde_json::to_string(&value)?;
    Ok(data)
}
fn append_encoded(tx: &Connection, event: &HarnessEvent, data: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO events(seq,session_id,run_id,created_at,kind,data) VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            event.sequence as i64,
            event.session_id,
            event.run_id,
            event.created_at,
            kind_name(&event.kind),
            data
        ],
    )?;
    Ok(())
}

fn hydrate(
    _store: &Store,
    conn: &Connection,
    operations: &serde_json::Value,
    data: &str,
) -> Result<HarnessEvent> {
    let mut value: serde_json::Value = serde_json::from_str(data)?;
    let Some(reference) = value.as_object_mut().and_then(|o| o.remove("_content_ref")) else {
        return Ok(serde_json::from_value(value)?);
    };
    let session_id = value["session_id"]
        .as_str()
        .context("Event session missing")?
        .to_owned();
    match serde_json::from_value::<ContentRef>(reference)? {
        ContentRef::ToolCall {
            message_id,
            call_id,
        } => {
            let message = Store::read_message(conn, &session_id, &message_id)?;
            let call = message
                .tool_calls
                .into_iter()
                .find(|call| call.id == call_id)
                .context("Event tool call missing")?;
            value["call"] = serde_json::to_value(call)?;
        }
        ContentRef::ToolResult { message_id } => {
            value["result"]["content"] = Store::read_message(conn, &session_id, &message_id)?
                .content
                .into();
        }
        ContentRef::Input { input_id: _ } => {
            let text = operations["text"]
                .as_str()
                .context("Input event text missing")?
                .to_owned();
            let message_id = operations["message_id"].as_str().map(str::to_owned);
            let content_blob = operations["content_blob"].as_str().map(str::to_owned);
            let content_format = operations["content_format"].as_str().map(str::to_owned);
            value["text"] = match message_id {
                Some(message_id) => Store::read_message(conn, &session_id, &message_id)?.content,
                None => Store::input_text(
                    conn,
                    &text,
                    content_blob.as_deref(),
                    content_format.as_deref(),
                )?,
            }
            .into();
        }
        ContentRef::Output { output_id } => {
            let data: String = conn.query_row(
                "SELECT data FROM outputs WHERE session_id=?1 AND id=?2",
                params![session_id, output_id],
                |row| row.get(0),
            )?;
            let record: crate::OutputRecord = serde_json::from_str(&data)?;
            value["record"]["output"] = record.output;
        }
        ContentRef::Approval { approval_id: _ } => {
            let approval = serde_json::from_value::<crate::ApprovalRecord>(operations.clone())?;
            value["approval"]["arguments"] = approval.arguments;
            value["approval"]["reason"] = if value["approval"]["state"] == "pending" {
                serde_json::Value::Null
            } else {
                serde_json::to_value(approval.reason)?
            };
        }
        ContentRef::ApprovalResolution { approval_id: _ } => {
            value["reason"] = serde_json::to_value(
                serde_json::from_value::<crate::ApprovalRecord>(operations.clone())?.reason,
            )?;
        }
        ContentRef::Run { run_id: _ } => {
            let data = operations["data"]
                .as_str()
                .context("Run event data missing")?
                .to_owned();
            let message_id = operations["result_message_id"].as_str().map(str::to_owned);
            let run = Store::run_from_data(conn, &data)?;
            let text = match message_id {
                Some(message_id) => {
                    Some(Store::read_message(conn, &session_id, &message_id)?.content)
                }
                None => run.text,
            };
            value["text"] = serde_json::to_value(text)?;
            value["error"] = serde_json::to_value(run.error)?;
            value["usage"] = serde_json::to_value(run.usage)?;
        }
        ContentRef::Blob { hash } => {
            let kind: serde_json::Value = serde_json::from_slice(&Store::blob_get(conn, &hash)?)?;
            for (key, item) in kind.as_object().context("Event blob is not an object")? {
                value[key] = item.clone();
            }
        }
    }
    Ok(serde_json::from_value(value)?)
}

fn hydrate_store(store: &Store, data: &str) -> Result<HarnessEvent> {
    let value: serde_json::Value = serde_json::from_str(data)?;
    let operations = match value
        .get("_content_ref")
        .cloned()
        .map(serde_json::from_value::<ContentRef>)
        .transpose()?
    {
        Some(ContentRef::Input { input_id }) => {
            let session = value["session_id"]
                .as_str()
                .context("Event session missing")?;
            let (text, message_id, content_blob, content_format) =
                store.input_event_row(session, &input_id)?;
            serde_json::json!({"text":text,"message_id":message_id,"content_blob":content_blob,"content_format":content_format})
        }
        Some(ContentRef::Run { run_id }) => {
            let (data, message_id) = store.run_row(&run_id)?;
            serde_json::json!({"data":data,"result_message_id":message_id})
        }
        Some(
            ContentRef::Approval { approval_id } | ContentRef::ApprovalResolution { approval_id },
        ) => serde_json::to_value(store.approval_record(&approval_id)?)?,
        _ => serde_json::Value::Null,
    };
    store.read(|conn| hydrate(store, conn, &operations, data))
}

/// The log's bounds at open: the last sequence and the last pruned sequence.
pub(crate) fn bounds(conn: &rusqlite::Connection) -> Result<(u64, u64)> {
    let last: Option<i64> = conn.query_row("SELECT MAX(seq) FROM events", [], |r| r.get(0))?;
    let pruned: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key=?1",
            [PRUNED_KEY],
            |r| r.get(0),
        )
        .optional()?;
    let pruned = pruned.and_then(|v| v.parse().ok()).unwrap_or(0);
    let remembered: Option<String> = conn
        .query_row("SELECT value FROM metadata WHERE key=?1", [LAST_KEY], |r| {
            r.get(0)
        })
        .optional()?;
    let remembered: u64 = remembered.and_then(|v| v.parse().ok()).unwrap_or(0);
    Ok(((last.unwrap_or(0).max(0) as u64).max(remembered), pruned))
}

/// Record the last assigned sequence, never lowering it.
pub(crate) fn remember_last(tx: &Connection, last: u64) -> Result<()> {
    tx.execute(
        "INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE CAST(value AS INTEGER)<CAST(excluded.value AS INTEGER)",
        params![LAST_KEY, last.to_string()],
    )?;
    Ok(())
}

/// Delete events older than the retention window, keeping the log a
/// contiguous suffix, and remember the last pruned sequence.
fn prune(store: &Store, days: u32) -> Result<()> {
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(days.into())).to_rfc3339();
    let through: Option<i64> = store.read(|conn| {
        Ok(conn.query_row(
            "SELECT MAX(seq) FROM events WHERE created_at<?1",
            [&cutoff],
            |r| r.get(0),
        )?)
    })?;
    let Some(through) = through else {
        return Ok(());
    };
    store.write(|tx| {
        tx.execute("DELETE FROM events WHERE seq<=?1", [through])?;
        tx.execute(
            "INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE CAST(value AS INTEGER)<CAST(excluded.value AS INTEGER)",
            params![PRUNED_KEY, through.to_string()],
        )?;
        Ok(())
    })?;
    let runtime = store.runtime();
    let mut state = runtime.log.state.lock().unwrap();
    state.pruned_through = state.pruned_through.max(through as u64);
    Ok(())
}

impl Harness {
    fn log(&self) -> std::sync::Arc<Log> {
        self.store().runtime().log.clone()
    }
    pub(crate) fn prune_events(&self, days: u32) {
        // A failed prune is retried at the next open; the log stays valid.
        let _ = prune(self.store(), days);
    }
    /// Durable events after `after`, ascending, at most `limit` (capped at
    /// 1000). With `session_id`, only that session's events.
    pub fn events_after(
        &self,
        after: u64,
        session_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<HarnessEvent>> {
        let limit = limit.min(EVENTS_AFTER_LIMIT) as i64;
        let after = after.min(i64::MAX as u64) as i64;
        let rows = self.store().read(|conn| {
            let mut statement = conn.prepare("SELECT data FROM events WHERE seq>?1 AND (?2 IS NULL OR session_id=?2) ORDER BY seq LIMIT ?3")?;
            Ok(statement.query_map(params![after, session_id, limit], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        rows.into_iter()
            .map(|data| {
                let value: serde_json::Value = serde_json::from_str(&data)?;
                let session = value["session_id"]
                    .as_str()
                    .context("Event session missing")?;
                let chat = self
                    .store()
                    .scoped(session)?
                    .unwrap_or_else(|| self.store().clone());
                hydrate_store(&chat, &data)
            })
            .collect()
    }
    /// The last durable sequence.
    pub fn last_sequence(&self) -> u64 {
        self.log().state.lock().unwrap().last
    }
    /// Events up to this sequence were pruned by retention. A client resuming
    /// from an older sequence must reload its snapshot instead.
    pub fn pruned_through(&self) -> u64 {
        let log = self.log();
        log.state
            .lock()
            .unwrap()
            .pruned_through
            .max(log.pruned_floor.load(Ordering::Relaxed))
    }
    /// Whether `events_after(after, ..)` returns every durable event after it.
    pub fn can_resume_from(&self, after: u64) -> bool {
        let log = self.log();
        let state = log.state.lock().unwrap();
        after
            >= state
                .pruned_through
                .max(log.pruned_floor.load(Ordering::Relaxed))
            && after <= state.last
    }
    /// Run store reads while no event is logged or broadcast, and return the
    /// log position they reflect: every durable event up to `seq` has its state
    /// change in the reads. Later events may also be reflected already (their
    /// state commits before they are logged), so clients apply them idempotently.
    /// Keep `read` to store reads; it must not wait on anything that emits.
    pub fn snapshot_with<T>(
        &self,
        session_id: &str,
        read: impl FnOnce() -> Result<T>,
    ) -> Result<(T, LogPosition)> {
        // A lazy chat open can recover runs and publish their parent events.
        // Finish that work before excluding event commits from this snapshot.
        self.store().scoped(session_id)?;
        let log = self.log();
        let state = log.state.lock().unwrap();
        let value = read()?;
        let position = LogPosition {
            seq: state.last,
            streaming_text: state
                .streaming
                .get(session_id)
                .map(|s| s.text.clone())
                .unwrap_or_default(),
            live_seen: state.live.get(session_id).copied().unwrap_or_default(),
        };
        Ok((value, position))
    }
}

/// Parse an optional `client` parameter of a wire command.
pub fn client_param(value: Option<&serde_json::Value>) -> Result<Option<ClientInfo>> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => {
            let client: ClientInfo = serde_json::from_value(value.clone())?;
            ensure!(
                !client.id.is_empty() && client.id.len() <= 256,
                "client.id must be 1 to 256 bytes"
            );
            ensure!(
                client.name.as_ref().is_none_or(|name| name.len() <= 256),
                "client.name must be at most 256 bytes"
            );
            Ok(Some(client))
        }
    }
}

#[cfg(test)]
#[path = "event_log_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "event_log_parent_tests.rs"]
mod parent_tests;
