//! Host-owned, reusable child sessions and a shared bounded registry.
pub use crate::profiles::AgentProfile;
use crate::{
    child_workspaces::{ChildRuntime, WorkspaceHost},
    codemode::CodeMode,
    config::Agents,
    question::{QuestionTools, answer_has_pending_input, pending_question},
    tools::ProcessTools,
    wake::{WakeCoordinator, WakeRegistration},
};
use anyhow::{Result, anyhow, bail, ensure};
use async_trait::async_trait;
use bubblepaw_core::{
    Approval, ApprovalMode, ApprovalPolicy, ApprovalResolver, CancellationToken, EventKind,
    Harness, HarnessConfig, HarnessEvent, JsonSchema, MessageOrigin, Role, Run, RunOptions,
    Session, Store, Tool, ToolContext, ToolDefinition, ToolDispatcher, ToolRegistry, ToolResult,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Notify, broadcast};

const MAX_CONTEXT_BYTES: usize = 64 * 1024;
const MAX_REPLY_BYTES: usize = 60 * 1024;
const BINDING_ID: &str = "bubblepaw-agent-binding-v1";
const CANCEL_ID: &str = "bubblepaw-agent-cancel-v1";
const READ_PREFIX: &str = "bubblepaw-agent-read-v1:";
const FAILURE_PREFIX: &str = "bubblepaw-agent-failure-v1:";
const CONTROLS_PREFIX: &str = "bubblepaw-agent-controls-v1:";
const CANCEL_PREFIX: &str = "bubblepaw-agent-cancel-run-v1:";
const WORKER_FINISHED_ID: &str = "bubblepaw-worker-finished-v1";

type ProfileAvailability = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// All managers for one host share this registry, including nested profiles.
#[derive(Default)]
pub struct AgentRegistry {
    output: crate::cli_output::Output,
    scope: Option<crate::session_scope::SessionScope>,
    state: Mutex<State>,
    wake: Arc<WakeCoordinator>,
    limits: crate::agent_limits::AgentLimits,
    live_approval: AtomicBool,
    approval_sources: Mutex<HashMap<String, ApprovalResolver>>,
    profile_availability: OnceLock<ProfileAvailability>,
    questions: OnceLock<Arc<QuestionTools>>,
    workspace_host: OnceLock<WorkspaceHost>,
}
#[derive(Default)]
struct State {
    stopping: bool,
    children: HashMap<String, Arc<Child>>,
    wake_registrations: HashMap<String, WakeRegistration>,
    recovery_checked: HashSet<String>,
}
struct Child {
    owner: String,
    session_id: String,
    handle: String,
    created: Instant,
    store: Store,
    harness: Arc<Harness>,
    runtime: Option<Arc<ChildRuntime>>,
    binding: Binding,
    /// False for a terminal worker opened with the inspection-only fallback.
    original_profile: bool,
    hidden: bool,
    depth: usize,
    turn: Mutex<Turn>,
    changed: Notify,
    wake: Weak<WakeCoordinator>,
}
struct Turn {
    result: Option<Value>,
    unread: bool,
    run_id: Option<String>,
    stop: CancellationToken,
    cancelling: bool,
    controls: Controls,
    approval: ApprovalPolicy,
    control_error: Option<String>,
    started: Instant,
    last_interaction: Instant,
    last_reminder: Option<Instant>,
    active_waits: usize,
}
struct CancelledTurn {
    child: Arc<Child>,
    run_id: Option<String>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Controls {
    output_schema: Option<JsonSchema>,
    max_tool_calls: Option<usize>,
    timeout: Option<u64>,
}
impl Controls {
    fn parse(input: &Value, retained_schema: Option<JsonSchema>) -> Result<Self> {
        let output_schema = match input.get("output_schema") {
            None => retained_schema,
            Some(Value::Null) => None,
            Some(value) => {
                ensure!(
                    value
                        .as_object()
                        .is_some_and(|v| v.keys().all(|k| matches!(k.as_str(), "name" | "schema"))),
                    "output_schema accepts only name and schema"
                );
                let schema: JsonSchema = serde_json::from_value(value.clone())?;
                schema.validate()?;
                Some(schema)
            }
        };
        let integer = |name: &str| -> Result<Option<u64>> {
            input
                .get(name)
                .map(|v| {
                    v.as_u64()
                        .ok_or_else(|| anyhow!("{name} must be a nonnegative integer"))
                })
                .transpose()
        };
        let max_tool_calls = integer("max_tool_calls")?
            .map(usize::try_from)
            .transpose()?;
        Ok(Self {
            output_schema,
            max_tool_calls,
            timeout: integer("timeout")?,
        })
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct Binding {
    profile: String,
    model: Option<String>,
    #[serde(default)]
    max_rounds: Option<usize>,
    approval: Option<ApprovalPolicy>,
    #[serde(default)]
    worker: bool,
}
#[derive(Serialize, Deserialize)]
struct WorkerFinished {
    run_id: String,
    status: String,
    finished_at: String,
}

mod controlled_prompt;
#[path = "agents_question.rs"]
mod questions;
#[path = "agent_reminders.rs"]
mod reminder_notices;
use controlled_prompt::controlled_prompt;
mod worktree_cleanup;
use worktree_cleanup::report_worktree_cleanup;
mod children;
mod helpers;
mod observe;
mod send;
mod setup;
mod start;
mod wake;
mod workers;
pub use helpers::clamp_approval;
use helpers::{
    address, cap, cleanup_workers_at, minimum, run_options, snapshot, subtree_ids,
    validate_profiles, worker_finished, workspace_reply,
};
mod waiting;
use waiting::{
    WaitInteraction, add_json, await_cancelled_turn, await_child, bounded_reply, cancelled_run,
    capture_partial, close_environments, load_controls, restore_text, save_controls,
    terminal_reply, touch,
};
mod tool;
use tool::AgentTool;
pub(crate) use tool::Manager;

pub struct AgentTools {
    store: Store,
    // Set once. A recursive host builds profiles after its own Arc exists.
    profiles: OnceLock<Vec<AgentProfile>>,
    repl: Option<Arc<CodeMode>>,
    processes: Option<Arc<ProcessTools>>,
    registry: Arc<AgentRegistry>,
    // Nested harness capabilities hold weak managers; the root owns their lifetime.
    _nested: Vec<Arc<AgentTools>>,
    cancel_on_drop: bool,
}
impl AgentRegistry {
    pub fn with_output(mut self, output: crate::cli_output::Output) -> Self {
        self.output = output;
        self
    }

    pub fn with_scope(mut self, scope: crate::session_scope::SessionScope) -> Self {
        self.scope = Some(scope);
        self
    }

    pub fn with_wake(mut self, wake: Arc<WakeCoordinator>) -> Self {
        self.wake = wake;
        self
    }
    pub fn wake_coordinator(&self) -> Arc<WakeCoordinator> {
        self.wake.clone()
    }
    fn signal(&self, store: &Store, owner: &str, include_owner: bool) -> Vec<CancelledTurn> {
        let mut ids = subtree_ids(&store.list_sessions().unwrap_or_default(), owner);
        for id in &ids {
            if include_owner || id != owner {
                self.wake.suppress(id);
            }
        }
        let Ok(state) = self.state.lock() else {
            return Vec::new();
        };
        // Catalog ancestry is authoritative, so idle descendants that were
        // evicted from the handle registry still bound the subtree. Registry
        // owners are merged in case the catalog read fails.
        loop {
            let before = ids.len();
            for child in state.children.values() {
                if ids.contains(&child.owner) {
                    ids.insert(child.session_id.clone());
                }
            }
            if before == ids.len() {
                break;
            }
        }
        let mut children = state
            .children
            .values()
            .filter(|c| ids.contains(&c.session_id) && (include_owner || c.session_id != owner))
            .cloned()
            .collect::<Vec<_>>();
        children.sort_by_key(|c| std::cmp::Reverse(c.depth));
        let children = children
            .into_iter()
            .map(|child| {
                let run_id = child.turn.lock().ok().and_then(|mut turn| {
                    turn.cancelling = true;
                    turn.stop.cancel();
                    turn.run_id.clone()
                });
                CancelledTurn { child, run_id }
            })
            .collect();
        // Each observer owns cancellation of its current run. Calling the
        // harness again here after releasing the registry lock could race a
        // completed observer and cancel a newly admitted continuation.
        drop(state);
        children
    }
    async fn finish_cancel(children: Vec<CancelledTurn>) {
        for cancelled in children {
            let child = cancelled.child;
            // Running turn observers own cancellation/cleanup. Idle sessions have
            // already released their resources; wait for descendants first.
            let _ = await_cancelled_turn(&child, &cancelled.run_id).await;
            if let Ok(mut turn) = child.turn.lock()
                && turn.run_id == cancelled.run_id
            {
                if let Some(reply) = turn.result.as_mut()
                    && reply["status"] == "idle"
                {
                    reply["status"] = json!("cancelled");
                }
                turn.cancelling = false;
            }
            child.changed.notify_waiters();
        }
    }
    async fn cancel_tree(&self, store: &Store, owner: &str, include_owner: bool) {
        Self::finish_cancel(self.signal(store, owner, include_owner)).await;
    }
    async fn cancel_children(&self, store: &Store, owner: &str) {
        self.cancel_tree(store, owner, false).await;
    }
}
fn child_notice_key(session: &str, run: &str) -> String {
    format!("child:{session}:{run}")
}
fn child_notice(child: &Child, run: &str, reply: &Value) -> String {
    let mut text = reply["text"]
        .as_str()
        .unwrap_or("")
        .chars()
        .take(4000)
        .collect::<String>();
    if reply["text"]
        .as_str()
        .is_some_and(|full| full.len() > text.len())
    {
        text.push_str(" [preview truncated; read the child result for the full answer]");
    }
    format!(
        "Child {} (sessionId {}, runId {}) ended with status {}.{}{}",
        child.handle,
        child.session_id,
        run,
        reply["status"].as_str().unwrap_or("failed"),
        if text.is_empty() { "" } else { "\n" },
        text
    )
}
impl Drop for AgentTools {
    fn drop(&mut self) {
        if !self.cancel_on_drop {
            return;
        }
        if let Ok(state) = self.registry.state.lock() {
            for child in state.children.values().filter(|child| {
                self.profiles()
                    .iter()
                    .any(|p| Arc::ptr_eq(&p.harness, &child.harness))
            }) {
                if let Ok(turn) = child.turn.lock()
                    && turn.result.is_none()
                {
                    turn.stop.cancel();
                }
            }
        }
    }
}
#[cfg(test)]
mod quota_resume_tests;

#[cfg(test)]
mod control_notice_tests;
