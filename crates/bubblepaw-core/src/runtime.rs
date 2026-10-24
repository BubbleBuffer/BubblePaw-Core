use crate::event_log::ContentRef;
use crate::{
    CancellationToken, ClientInfo, EventKind, HarnessEvent, InputKind, Message, ModelRequest,
    ModelResponse, Observation, ObservationScope, Provider, QueuedInput, RequestKind, RequestUsage,
    Role, Run, RunOptions, Store, TextCallback, ToolCall, ToolContext, ToolDispatcher,
    ToolRegistry, ToolResult, Usage, capture_request_usage_with_store, id, now, stream::Paragraphs,
};
use anyhow::{Context, Result, anyhow, ensure};
use async_trait::async_trait;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Notify, broadcast};

#[path = "runtime_admission.rs"]
mod runtime_admission;
#[path = "runtime_context.rs"]
mod runtime_context;
#[path = "runtime_mode.rs"]
mod runtime_mode;
pub use runtime_context::FullHistory;
#[path = "runtime_catalog.rs"]
mod runtime_catalog;
#[path = "runtime_compact.rs"]
mod runtime_compact;
#[path = "runtime_preparation.rs"]
mod runtime_preparation;
pub use runtime_preparation::{
    ContextBudgetExceeded, ContextPreparation, PrepareCall, PrepareMode, PreparedContext,
};
#[path = "runtime_reminders.rs"]
mod runtime_reminders;
#[path = "runtime_result.rs"]
mod runtime_result;
#[path = "runtime_supervision.rs"]
mod runtime_supervision;
#[path = "runtime_tools.rs"]
mod runtime_tools;
use crate::submit_result::is_submission;

/// Resolve the host's default model and request client as one snapshot.
pub type ProviderResolver = Arc<dyn Fn() -> (String, Arc<dyn Provider>) + Send + Sync>;
/// A host-owned registry snapshot, captured at each turn start and explicit refresh.
pub type ToolRegistryResolver = Arc<dyn Fn() -> ToolRegistry + Send + Sync>;
/// Resolve approval policy at each tool call, including composed host calls.
pub type ApprovalResolver = Arc<dyn Fn(&str) -> crate::ApprovalPolicy + Send + Sync>;
/// The directory a session's exec tool runs in, such as its worktree. None uses
/// its root chat's persisted workspace, which the hardline floor also protects.
/// Old chats without a persisted binding retain the store's legacy default.
pub type WorkspaceResolver = Arc<dyn Fn(&str) -> Option<std::path::PathBuf> + Send + Sync>;
/// Host-owned session instructions, resolved over the selected base prompt.
pub type SystemPromptResolver = Arc<dyn Fn(&str, &str) -> Result<String> + Send + Sync>;

/// Optional host work before a turn captures its tools or makes a model request.
#[async_trait]
pub trait TurnStart: Send + Sync {
    async fn before_turn(&self, session_id: &str, cancellation: CancellationToken) -> Result<()>;
}

#[derive(Clone)]
pub struct HarnessConfig {
    pub execution_mode: crate::ExecutionMode,
    pub model: String,
    /// Resolve before context preparation; explicit run model overrides stay pinned.
    pub provider_resolver: Option<ProviderResolver>,
    pub registry_resolver: Option<ToolRegistryResolver>,
    pub turn_start: Option<Arc<dyn TurnStart>>,
    pub system_prompt: String,
    pub system_prompt_resolver: Option<SystemPromptResolver>,
    pub max_request_bytes: usize,
    pub max_tool_result_bytes: usize,
    pub max_paragraph_bytes: usize,
    /// Optional model/tool round limit; None allows the run to finish naturally.
    pub max_rounds: Option<usize>,
    pub event_capacity: usize,
    pub context: Arc<dyn ContextPreparation>,
    /// Disabled by default. Observations are disposable and never stored in the archive.
    pub observation: Option<Observation>,
    /// Which tool calls wait for a person; RunOptions may replace it per run.
    pub approval: crate::ApprovalPolicy,
    /// Explicit RunOptions approval overrides remain pinned for the whole run.
    pub approval_resolver: Option<ApprovalResolver>,
    /// Host-resolved exec directory per session, for the hardline workspace checks.
    pub workspace_resolver: Option<WorkspaceResolver>,
    /// Host-resolved approver role; absent means Auto falls through to Ask.
    pub auto_approval: Option<crate::AutoApprovalConfig>,
    /// Delete logged events older than this many days when a harness opens.
    /// None keeps everything (docs/session-api.md).
    pub event_retention_days: Option<u32>,
}
impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            execution_mode: crate::ExecutionMode::default(),
            model: "default".into(),
            provider_resolver: None,
            registry_resolver: None,
            turn_start: None,
            system_prompt: "You are a helpful assistant.".into(),
            system_prompt_resolver: None,
            max_request_bytes: 2 * 1024 * 1024,
            max_tool_result_bytes: 64 * 1024,
            max_paragraph_bytes: 16 * 1024,
            max_rounds: None,
            event_capacity: 256,
            context: Arc::new(FullHistory),
            observation: None,
            approval: crate::ApprovalPolicy::default(),
            approval_resolver: None,
            workspace_resolver: None,
            auto_approval: None,
            event_retention_days: None,
        }
    }
}
pub(crate) struct Active {
    run_id: String,
    cancellation: CancellationToken,
    notify: Arc<Notify>,
    finished: Option<Run>,
    /// The turn panicked; `finished` is the failed record every waiter reports.
    panicked: Option<String>,
    /// The settled boundary is committed; maintenance precedes idle notification.
    finalizing: bool,
    /// The client whose cancel stopped this run; RunFinished carries it.
    cancelled_by: Option<ClientInfo>,
}
struct CallbackLifetime(Arc<AtomicBool>);
struct TurnOutcome {
    text: String,
    result_message_id: String,
    final_message: Option<Message>,
}
struct SingleToolRun {
    call: ToolCall,
    result: Arc<Mutex<Option<ToolResult>>>,
    completion: tokio::sync::oneshot::Sender<Run>,
}
impl Drop for CallbackLifetime {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}
#[derive(Default)]
pub(crate) struct RuntimeState {
    active: Mutex<HashMap<String, Active>>,
    compacting: Mutex<HashMap<String, CancellationToken>>,
    finalized: std::sync::Condvar,
    stopping: AtomicBool,
    pub(crate) approvals: crate::approvals::Waiters,
    /// One event sequence and broadcast per store (docs/session-api.md).
    pub(crate) log: Arc<crate::event_log::Log>,
    histories: Mutex<HashMap<String, ParsedHistory>>,
    /// The one prompt-cache ledger of this store; its `context` is the root lineage.
    pub(crate) cache: Arc<crate::CacheAccounting>,
    pub(crate) session_cleanups: Mutex<HashMap<String, crate::store::SessionCleanup>>,
}
/// Existing inventory body header, retained as frozen text. Reminder identity
/// and restart deduplication use structured runtime_inventory metadata.
pub const RUNTIME_INVENTORY_HEADER: &str = "Current volatile runtime facts";

#[derive(Default)]
struct ParsedHistory {
    frontier: Option<i64>,
    prefix_id: Option<String>,
    through: i64,
    messages: Vec<Message>,
    sizes: HashMap<String, (Message, usize)>,
}
impl ParsedHistory {
    fn refresh(&mut self, store: &Store, session_id: &str) -> Result<()> {
        for (seq, message) in store.messages_after(session_id, self.through)? {
            self.through = seq;
            self.messages.push(message);
        }
        Ok(())
    }
    fn request_bytes(&mut self, request: &mut ModelRequest) -> Result<usize> {
        // Serializing a message-free shell preserves the exact serde wire size,
        // including optional fields. Canonical-only chat journals are excluded
        // from message payload accounting. Existing messages are only compared and
        // counted; transformed/new messages are serialized once.
        let messages = std::mem::take(&mut request.messages);
        let shell = serde_json::to_vec(request).map(|bytes| bytes.len());
        request.messages = messages;
        let mut bytes = shell?;
        for message in &request.messages {
            let entry = self
                .sizes
                .entry(message.id.clone())
                .or_insert_with(|| (message.clone(), 0));
            if entry.1 == 0 || !runtime_admission::same_message_wire_size(&entry.0, message) {
                entry.1 = crate::request_size::prepared_message_bytes(message)?;
                entry.0 = message.clone();
            }
            bytes = bytes.saturating_add(entry.1);
        }
        Ok(bytes.saturating_add(request.messages.len().saturating_sub(1)))
    }
}
impl RuntimeState {
    pub(crate) fn quota_cancellation(&self, session: &str, run: &str) -> Option<CancellationToken> {
        self.active
            .lock()
            .unwrap()
            .get(session)
            .filter(|active| active.run_id == run)
            .map(|active| active.cancellation.clone())
    }
    fn release_history(&self, session: &str) {
        if let Some(history) = self.histories.lock().unwrap().get_mut(session) {
            *history = ParsedHistory {
                ..Default::default()
            };
        }
    }
    pub(crate) fn forget_history(&self, session: &str) {
        self.histories.lock().unwrap().remove(session);
        self.cache.context.release_session(session);
        self.log.forget_session(session);
    }
    fn admission_guard(&self, session: &str) -> std::sync::MutexGuard<'_, HashMap<String, Active>> {
        let mut active = self.active.lock().unwrap();
        while active
            .get(session)
            .is_some_and(|run| run.finalizing && run.finished.is_none())
        {
            active = self.finalized.wait(active).unwrap();
        }
        active
    }
    pub(crate) fn delete_guard(
        &self,
        session_ids: &[String],
    ) -> Result<std::sync::MutexGuard<'_, HashMap<String, Active>>> {
        let active = self.active.lock().unwrap();
        ensure!(
            !session_ids.iter().any(|session| active
                .get(session)
                .is_some_and(|run| run.finished.is_none())),
            "Cannot delete a session with an active run"
        );
        ensure!(
            !session_ids.iter().any(|session| self
                .compacting
                .lock()
                .unwrap()
                .contains_key(session)),
            "Cannot delete a session during compaction"
        );
        Ok(active)
    }
    pub(crate) fn fork_guard(
        &self,
        session_id: &str,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<String, Active>>> {
        let active = self.active.lock().unwrap();
        ensure!(
            !active.get(session_id).is_some_and(|a| a.finished.is_none()),
            "Cannot fork an active session"
        );
        ensure!(
            !self.compacting.lock().unwrap().contains_key(session_id),
            "Cannot fork a session during compaction"
        );
        Ok(active)
    }
}

/// Release even when a host drops the compaction future or an adapter fails.
struct CompactionReservation {
    state: Arc<RuntimeState>,
    session: String,
}
impl Drop for CompactionReservation {
    fn drop(&mut self) {
        self.state.compacting.lock().unwrap().remove(&self.session);
    }
}

/// `Harness::start` on a session that already has an active run. Hosts
/// downcast the `anyhow::Error` to this and steer or follow up instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBusy {
    pub session_id: String,
    pub run_id: String,
}
impl std::fmt::Display for SessionBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Session already has an active run {}; steer or follow up instead",
            self.run_id
        )
    }
}
impl std::error::Error for SessionBusy {}

#[derive(Clone)]
pub struct Harness {
    store: Store,
    provider: Arc<dyn Provider>,
    tools: ToolRegistry,
    config: HarnessConfig,
    state: Arc<RuntimeState>,
    cursors: Arc<crate::provider_ext::HistoryTracker>,
}
impl Harness {
    fn history(&self, session_id: &str) -> Result<Vec<Message>> {
        let host_frontier = self.config.context.history_frontier(session_id)?;
        let (frontier, prefix) = if host_frontier.is_some() {
            (host_frontier, Vec::new())
        } else {
            self.store.imported_history_seed(session_id)?
        };
        let mut histories = self.state.histories.lock().unwrap();
        let history = histories.entry(session_id.into()).or_default();
        let prefix_id = prefix.first().map(|message| message.id.clone());
        if history.frontier != frontier || history.prefix_id != prefix_id {
            *history = ParsedHistory {
                through: frontier.unwrap_or(0),
                frontier,
                prefix_id,
                messages: prefix,
                ..Default::default()
            };
        }
        history.refresh(&self.store, session_id)?;
        Ok(history.messages.clone())
    }
    fn latest_user_request(&self, session_id: &str) -> Result<String> {
        let is_user_request = |message: &Message| {
            message.role == Role::User && message.origin != Some(crate::MessageOrigin::Generated)
        };
        let history = self.history(session_id)?;
        if let Some(message) = history.into_iter().rev().find(is_user_request) {
            return Ok(message.content);
        }
        Ok(self
            .store
            .context_view(session_id)?
            .and_then(|view| view.prefix.into_iter().rev().find(is_user_request))
            .map(|message| message.content)
            .unwrap_or_default())
    }
    pub fn new(
        store: Store,
        provider: Arc<dyn Provider>,
        tools: ToolRegistry,
        config: HarnessConfig,
    ) -> Self {
        let state = store.runtime();
        state.log.init(config.event_capacity);
        let harness = Self {
            store,
            provider,
            tools,
            config,
            state,
            cursors: Arc::default(),
        };
        if let Some(days) = harness.config.event_retention_days {
            harness.prune_events(days);
        }
        harness
    }
    pub fn store(&self) -> &Store {
        &self.store
    }
    /// Clone this harness with a host-decorated provider, preserving its store,
    /// tools, configuration and shared runtime. The host owns decorator policy.
    pub fn map_provider(
        &self,
        map: impl Fn(Arc<dyn Provider>) -> Arc<dyn Provider> + Send + Sync + 'static,
    ) -> Self {
        let mut harness = self.clone();
        let map = Arc::new(map);
        harness.provider = map(self.provider.clone());
        if let Some(resolve) = &self.config.provider_resolver {
            let resolve = resolve.clone();
            harness.config.provider_resolver = Some(Arc::new(move || {
                let (model, provider) = resolve();
                (model, map(provider))
            }));
        }
        harness
    }
    /// Default model/tool round limit; individual runs may override it.
    pub fn max_rounds(&self) -> Option<usize> {
        self.config.max_rounds
    }
    /// Events of every harness on this store, child agents included.
    pub fn subscribe(&self) -> broadcast::Receiver<HarnessEvent> {
        self.state.log.subscribe()
    }
    pub(crate) fn emit(&self, session_id: &str, run_id: &str, kind: EventKind) {
        self.emit_from(session_id, run_id, kind, None);
    }
    pub(crate) fn emit_from(
        &self,
        session_id: &str,
        run_id: &str,
        kind: EventKind,
        origin: Option<ClientInfo>,
    ) {
        self.state
            .log
            .emit(&self.store, session_id, run_id, kind, origin);
    }
    fn new_run(&self, session_id: &str, options: &RunOptions) -> Run {
        Run {
            id: id(),
            session_id: session_id.into(),
            model: if options.script.is_some() {
                crate::SCRIPT_MODEL.into()
            } else {
                options
                    .model
                    .clone()
                    .unwrap_or_else(|| self.default_model())
            },
            status: "running".into(),
            text: None,
            error: None,
            started_at: now(),
            ended_at: None,
            usage: None,
        }
    }
    fn default_model(&self) -> String {
        self.provider_binding().0
    }
    fn provider_binding(&self) -> (String, Arc<dyn Provider>) {
        self.config.provider_resolver.as_ref().map_or_else(
            || (self.config.model.clone(), self.provider.clone()),
            |resolve| resolve(),
        )
    }
    /// Returns only after input and the running record are committed, then starts execution.
    pub fn start(
        &self,
        session_id: &str,
        text: impl Into<String>,
        options: RunOptions,
    ) -> Result<Run> {
        self.admit(session_id, Some(text.into()), options, false)
    }
    /// Admit only when the session's storage is already active. The availability
    /// check shares the admission lock with archiving/deletion, so this entry
    /// point never restores an archive as a side effect of starting work.
    pub fn start_if_active(
        &self,
        session_id: &str,
        text: impl Into<String>,
        options: RunOptions,
    ) -> Result<Run> {
        self.admit(session_id, Some(text.into()), options, true)
    }
    pub fn start_resume(&self, session_id: &str, options: RunOptions) -> Result<Run> {
        self.admit(session_id, None, options, false)
    }
    /// Admit an ordinary new run only if this exact interrupted quota wait is
    /// still pending and due. The ownership check and marker removal share the
    /// admission transaction, so cancellation cannot race a scheduled resume.
    pub fn start_quota_resume(
        &self,
        expected: &crate::QuotaWait,
        options: RunOptions,
    ) -> Result<Run> {
        self.admit_execution(
            &expected.session_id,
            None,
            options,
            false,
            None,
            None,
            Some(expected),
            Vec::new(),
        )
    }
    pub async fn prompt(
        &self,
        session_id: &str,
        text: impl Into<String>,
        options: RunOptions,
    ) -> Result<Run> {
        self.start(session_id, text, options)?;
        self.wait_for_idle(session_id).await
    }
    /// Execute one registered tool, including deferred tools, without a model turn.
    /// admission, validation, approval, cancellation, result limits and journals.
    /// Interactive approval fails immediately, including calls nested in `js`.
    /// Tool failures return `Ok` with `is_error`; admission, infrastructure and
    /// cancellation failures return `Err`. Queued input remains for a future run.
    pub async fn call_tool(
        &self,
        session_id: &str,
        name: &str,
        arguments: serde_json::Value,
        options: RunOptions,
        cancellation: CancellationToken,
    ) -> Result<ToolResult> {
        ensure!(
            options.script.is_none(),
            "call_tool cannot be combined with RunOptions.script; call js with code arguments instead"
        );
        ensure!(
            options.output_schema.is_none(),
            "output_schema requires a model run"
        );
        ensure!(
            !cancellation.is_cancelled(),
            "Cancelled before tool admission"
        );
        let (completion, completed) = tokio::sync::oneshot::channel();
        let result = Arc::default();
        let execution = SingleToolRun {
            call: ToolCall {
                id: format!("tool_{}", id()),
                name: name.into(),
                arguments,
            },
            result: Arc::clone(&result),
            completion,
        };
        self.admit_execution(
            session_id,
            None,
            options,
            false,
            Some(execution),
            Some(cancellation),
            None,
            Vec::new(),
        )?;
        let run = completed
            .await
            .context("Tool run ended without a completion record")?;
        let result = result.lock().unwrap().take();
        let result = result.ok_or_else(|| {
            anyhow!(
                "{}",
                run.error
                    .as_deref()
                    .unwrap_or("Tool run finished without a result")
            )
        })?;
        // An ordinary tool error fails the durable run with the same body. A
        // later checkpoint failure or cancellation must still reach the caller.
        ensure!(
            run.status == "completed"
                || (result.is_error
                    && run.status == "failed"
                    && run.error.as_deref() == Some(result.content.as_str())),
            "{}",
            run.error.as_deref().unwrap_or("Tool run did not complete")
        );
        Ok(result)
    }
    pub async fn resume(&self, session_id: &str, options: RunOptions) -> Result<Run> {
        self.start_resume(session_id, options)?;
        self.wait_for_idle(session_id).await
    }
    pub fn cancel(&self, session_id: &str) -> bool {
        self.cancel_from(session_id, None)
    }
    /// Delete a session's stored history (`Store::delete_session`) and drop
    /// the provider's mirror of it. Refused while the session has an active run.
    pub async fn delete_session(&self, session_id: &str) -> Result<()> {
        {
            let active = self.state.active.lock().unwrap();
            if let Some(running) = active.get(session_id).filter(|a| a.finished.is_none()) {
                return Err(SessionBusy {
                    session_id: session_id.into(),
                    run_id: running.run_id.clone(),
                }
                .into());
            }
        }
        // Store deletion takes the same admission guard (including descendants
        // for a shared chat). Do not recursively lock it here.
        self.store.delete_session(session_id)?;
        self.release_deleted_session(session_id).await
    }
    /// Release host/provider caches after a store-level deletion has committed.
    pub async fn release_deleted_session(&self, session_id: &str) -> Result<()> {
        ensure!(
            !self
                .store
                .list_sessions()?
                .iter()
                .any(|session| session.id == session_id),
            "Session still exists"
        );
        self.state.active.lock().unwrap().remove(session_id);
        self.state.log.forget_session(session_id);
        self.state.histories.lock().unwrap().remove(session_id);
        self.release_context_state(session_id);
        self.cursors.forget(session_id);
        self.config.context.release_session(session_id);
        // The history is gone either way; the mirror is dropped best effort.
        self.provider.close_session(session_id).await
    }
    /// Release disposable caches and a provider mirror after a turn is idle,
    /// without changing its canonical history or retained completion metadata.
    pub async fn release_idle_resources(&self, session_id: &str) -> Result<()> {
        {
            let active = self.state.active.lock().unwrap();
            ensure!(
                active
                    .get(session_id)
                    .is_none_or(|run| run.finished.is_some()),
                "Session has an active run"
            );
        }
        self.state.release_history(session_id);
        self.cursors.forget(session_id);
        self.config.context.release_session(session_id);
        self.provider.close_session(session_id).await
    }
    /// Cancel, recording the client; the run's RunFinished carries it as origin.
    pub fn cancel_from(&self, session_id: &str, client: Option<ClientInfo>) -> bool {
        let mut active = self.state.admission_guard(session_id);
        let cancelled_compaction = self
            .state
            .compacting
            .lock()
            .unwrap()
            .get(session_id)
            .map(|token| {
                token.cancel();
                true
            })
            .unwrap_or(false);
        let run_id = active
            .get(session_id)
            .map(|value| value.run_id.clone())
            .unwrap_or_default();
        let cancelled_run =
            if let Some(value) = active.get_mut(session_id).filter(|a| a.finished.is_none()) {
                if !value.cancellation.is_cancelled() {
                    value.cancelled_by = client;
                }
                value.cancellation.cancel();
                true
            } else {
                false
            };
        // A host hook may unblock a tool or provider and wait for its result.
        // Signal the run first so that work cannot finish as a successful turn,
        // then release admission before hooks recurse into child cancellation.
        // A restored quota marker has no live future: cancel its marker while
        // admission is still excluded so a scheduler cannot win in between.
        let cancelled_quota = if cancelled_run {
            self.store.clear_cancelled_quota_wait(session_id, &run_id)
        } else {
            self.store.cancel_quota_wait(session_id)
        };
        drop(active);
        let cancelled_background = self
            .tool_snapshot(session_id)
            .is_ok_and(|tools| tools.cancel_session(session_id) > 0);
        if let Some(observer) = &self.config.observation {
            if let Err(error) = &cancelled_quota {
                observer.failure(
                    ObservationScope {
                        session_id: session_id.into(),
                        run_id: run_id.clone(),
                        ..Default::default()
                    },
                    "quota_cancel_failed",
                    0,
                    "storage_error",
                    &error.to_string(),
                );
            }
            observer.emit(ObservationScope { session_id:session_id.into(),run_id,..Default::default() }, "cancel_intent", serde_json::json!({"cancelled_background":cancelled_background,"active_run":cancelled_run}), None);
        }
        // Signalling a volatile token is insufficient when a durable quota
        // hint could restart the task. The caller must retry a failed cancel.
        cancelled_quota.is_ok_and(|quota| {
            cancelled_run || cancelled_background || cancelled_compaction || quota
        })
    }
    pub fn steer(&self, session_id: &str, text: impl Into<String>) -> Result<QueuedInput> {
        self.steer_from(session_id, text, None)
    }
    pub fn followup(&self, session_id: &str, text: impl Into<String>) -> Result<QueuedInput> {
        self.followup_from(session_id, text, None)
    }
    /// Steer, recording the client on SteeringAccepted.
    pub fn steer_from(
        &self,
        session_id: &str,
        text: impl Into<String>,
        client: Option<ClientInfo>,
    ) -> Result<QueuedInput> {
        self.accept(session_id, InputKind::Steer, text.into(), client)
    }
    /// Queue a followup, recording the client on FollowupQueued.
    pub fn followup_from(
        &self,
        session_id: &str,
        text: impl Into<String>,
        client: Option<ClientInfo>,
    ) -> Result<QueuedInput> {
        self.accept(session_id, InputKind::Followup, text.into(), client)
    }
    fn accept(
        &self,
        session_id: &str,
        kind: InputKind,
        text: String,
        client: Option<ClientInfo>,
    ) -> Result<QueuedInput> {
        let active = self.state.admission_guard(session_id);
        ensure!(
            !self.state.stopping.load(Ordering::Relaxed),
            "Harness is shutting down"
        );
        ensure!(
            text.len() <= self.config.max_request_bytes,
            "Input exceeds request size limit"
        );
        let run_id = active
            .get(session_id)
            .filter(|a| a.finished.is_none())
            .map(|a| a.run_id.clone());
        self.store.session(session_id)?;
        let input = QueuedInput {
            id: id(),
            session_id: session_id.into(),
            kind: kind.clone(),
            text: text.clone(),
            state: "pending".into(),
            origin: Some(crate::MessageOrigin::Typed),
        };
        self.state.log.commit(
            &self.store,
            session_id,
            run_id.as_deref().unwrap_or_default(),
            client,
            |tx| {
                Store::queue_tx(tx, &input)?;
                let events = run_id
                    .as_ref()
                    .map(|_| {
                        let kind = if kind == InputKind::Steer {
                            EventKind::SteeringAccepted { text: text.clone() }
                        } else {
                            EventKind::FollowupQueued { text: text.clone() }
                        };
                        vec![(
                            kind,
                            Some(ContentRef::Input {
                                input_id: input.id.clone(),
                            }),
                        )]
                    })
                    .unwrap_or_default();
                Ok(((), events))
            },
        )?;
        // Admission only queues input. Releasing foreground waits is an
        // explicit host action, independent of accepting ordinary steering.
        if let Some(observer) = &self.config.observation {
            observer.emit(ObservationScope { session_id:session_id.into(),run_id:run_id.clone().unwrap_or_default(),..Default::default() }, "input_accepted", serde_json::json!({"input_id":input.id,"input_kind":kind,"text_bytes":text.len(),"fingerprint":crate::fingerprint(text.as_bytes())}), Some(serde_json::json!({"text":text})));
        }
        Ok(input)
    }
    pub async fn shutdown(&self, timeout: Duration) -> Result<()> {
        self.state.stopping.store(true, Ordering::Relaxed);
        for token in self.state.compacting.lock().unwrap().values() {
            token.cancel();
        }
        let sessions: Vec<String> = {
            let active = self.state.active.lock().unwrap();
            active
                .iter()
                .filter(|(_, a)| a.finished.is_none())
                .map(|(id, a)| {
                    if !a.finalizing {
                        a.cancellation.cancel();
                    }
                    id.clone()
                })
                .collect()
        };
        tokio::time::timeout(timeout, async {
            for session in sessions {
                self.wait_for_idle(&session).await?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .map_err(|_| anyhow!("Shutdown timed out; cancellation requested"))??;
        Ok(())
    }
    async fn drive(
        &self,
        mut run: Run,
        mut options: RunOptions,
        cancellation: CancellationToken,
        mut single_tool: Option<SingleToolRun>,
    ) {
        loop {
            if let Some(observer) = &self.config.observation {
                observer.emit(
                    ObservationScope {
                        session_id: run.session_id.clone(),
                        run_id: run.id.clone(),
                        ..Default::default()
                    },
                    "run_started",
                    serde_json::json!({"model":run.model}),
                    None,
                );
            }
            let mut usage = None;
            let mut request_usage = Vec::new();
            let result = crate::quota::with_quota_wait_context(
                &self.store,
                &run.session_id,
                &run.id,
                self.turn(
                    &run,
                    &options,
                    &cancellation,
                    &mut usage,
                    &mut request_usage,
                    single_tool.as_ref(),
                ),
            )
            .await;
            let mut result_message_id = None;
            let mut final_message = None;
            match result {
                Ok(outcome) => {
                    run.status = "completed".into();
                    run.text = Some(outcome.text);
                    run.usage = Some(usage.unwrap_or_default());
                    result_message_id = Some(outcome.result_message_id);
                    final_message = outcome.final_message;
                }
                Err(error) => {
                    run.usage = usage;
                    run.status = if cancellation.is_cancelled() {
                        "cancelled"
                    } else {
                        "failed"
                    }
                    .into();
                    run.error = Some(format!("{error:#}"));
                    if let Err(storage_error) =
                        self.store.close_unresolved(&run.session_id, &run.id)
                    {
                        run.error = Some(format!("{error:#}; recovery failed: {storage_error}"));
                    }
                }
            }
            run.ended_at = Some(now());
            if let Some(observer) = &self.config.observation {
                if let Some(error) = &run.error {
                    observer.failure(
                        ObservationScope {
                            session_id: run.session_id.clone(),
                            run_id: run.id.clone(),
                            ..Default::default()
                        },
                        "run_failed",
                        0,
                        &run.status,
                        error,
                    );
                }
                observer.run_finished(
                    ObservationScope {
                        session_id: run.session_id.clone(),
                        run_id: run.id.clone(),
                        ..Default::default()
                    },
                    &run.status,
                    run.usage.as_ref(),
                );
            }
            let mut active = self.state.active.lock().unwrap();
            let origin = if run.status == "cancelled" {
                active
                    .get_mut(&run.session_id)
                    .and_then(|a| a.cancelled_by.take())
            } else {
                None
            };
            // Queue admission uses this same active lock. The pending check and
            // publishing the settled state therefore have no late-input window.
            let continue_run = run.status == "completed"
                && single_tool.is_none()
                && !cancellation.is_cancelled()
                && !self.state.stopping.load(Ordering::Relaxed)
                && self.store.has_pending(&run.session_id).unwrap_or(false);
            let checkpoint =
                self.state
                    .log
                    .commit(&self.store, &run.session_id, &run.id, origin, |tx| {
                        if let Some(message) = &final_message {
                            Store::insert_message(tx, message)?;
                        }
                        Store::insert_request_usage_tx(tx, &request_usage)?;
                        Store::finish_run_tx(tx, &run, result_message_id.as_deref())?;
                        let mut events = vec![(
                            EventKind::RunFinished {
                                status: run.status.clone(),
                                text: run.text.clone(),
                                error: run.error.clone(),
                                usage: run.usage.clone(),
                            },
                            Some(ContentRef::Run {
                                run_id: run.id.clone(),
                            }),
                        )];
                        if !continue_run {
                            events.push((EventKind::SessionSettled {}, None));
                        }
                        Ok(((), events))
                    });
            if let Err(error) = &checkpoint {
                run.status = "failed".into();
                run.error = Some(format!("Run checkpoint failed: {error}"));
            }
            if continue_run && checkpoint.is_ok() {
                // Input queued during a script run is answered by a model run.
                options.script = None;
                let next = self.new_run(&run.session_id, &options);
                if self
                    .state
                    .log
                    .commit(&self.store, &next.session_id, &next.id, None, |tx| {
                        Store::start_run_tx(tx, &next)?;
                        Store::consume_tx(tx, &next.session_id, false, true)?;
                        Ok((
                            (),
                            vec![(
                                EventKind::RunStarted {
                                    model: next.model.clone(),
                                },
                                None,
                            )],
                        ))
                    })
                    .is_ok()
                {
                    if let Some(value) = active.get_mut(&run.session_id) {
                        value.run_id = next.id.clone();
                    }
                    run = next;
                    drop(active);
                    continue;
                }
                self.emit(&run.session_id, &run.id, EventKind::SessionSettled {});
            }
            // Admission waits for this final phase without holding the active
            // lock, allowing archival to inspect other roots. Idle notification
            // still follows maintenance, and late input starts a future run.
            if let Some(value) = active.get_mut(&run.session_id) {
                value.finalizing = true;
            }
            drop(active);
            let _ = self.store.runtime_idle();
            let mut active = self.state.active.lock().unwrap();
            if let Some(value) = active.get_mut(&run.session_id) {
                let mut metadata = run.clone();
                metadata.text = None;
                value.finished = Some(metadata);
                value.notify.notify_waiters();
                self.state.finalized.notify_all();
            }
            drop(active);
            if let Some(execution) = single_tool.take() {
                let _ = execution.completion.send(run);
            }
            break;
        }
    }
    async fn turn(
        &self,
        run: &Run,
        options: &RunOptions,
        cancellation: &CancellationToken,
        usage: &mut Option<Usage>,
        request_usage: &mut Vec<RequestUsage>,
        single_tool: Option<&SingleToolRun>,
    ) -> Result<TurnOutcome> {
        ensure!(!cancellation.is_cancelled(), "Cancelled");
        if let Some(hook) = &self.config.turn_start {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(anyhow!("Cancelled during turn start")),
                result = hook.before_turn(&run.session_id, cancellation.clone()) => result?,
            }
        }
        ensure!(!cancellation.is_cancelled(), "Cancelled");
        // Capture the consumed run input before a provider or host can append
        // other messages. Nested calls retain the same approval scope.
        let mut user_request = if self.config.auto_approval.is_some() {
            self.latest_user_request(&run.session_id)?
        } else {
            String::new()
        };
        let snapshot = || self.run_tool_snapshot(&run.session_id, options);
        let mut tools = snapshot()?;
        let native = self.execution_mode(&run.session_id)? == crate::ExecutionMode::Native;
        let mut tool_surface = runtime_tools::RunToolSurface::load(self, &run.session_id, options)?;
        self.queue_result_schema_notice(run, options, &tools)?;
        let mut dispatcher = ToolDispatcher::new(
            self.store.clone(),
            tools.clone(),
            self.config.max_tool_result_bytes,
            self.config.observation.clone(),
        )
        .with_approval(
            options
                .approval
                .clone()
                .unwrap_or_else(|| self.config.approval.clone()),
        )
        .with_approval_resolver(match options.approval.clone() {
            None => self.config.approval_resolver.clone(),
            Some(override_policy) => {
                let resolve = self.config.approval_resolver.clone();
                let baseline = self.config.approval.clone();
                Some(Arc::new(move |session: &str| {
                    let current = resolve
                        .as_ref()
                        .map(|resolve| resolve(session))
                        .unwrap_or_else(|| baseline.clone());
                    // The host keeps its denies and hardline setting; the run keeps its modes.
                    override_policy
                        .clone()
                        .with_inherited_denies(&current)
                        .with_inherited_hardline(&current)
                }) as crate::ApprovalResolver)
            }
        })
        .with_workspace_resolver(self.config.workspace_resolver.clone())
        .with_tool_budget(options.max_tool_calls)
        .with_interactive_approval(single_tool.is_none());
        let max_rounds = options.max_rounds.or(self.config.max_rounds);
        let mut result_nudged = false;
        let mut result_retry_pending = false;
        let mut budget_final_requested = false;
        // Exhaustion gets a final tools-off request; a missing result gets one nudge.
        // An ignored tools-off reminder gets one enforcement round after them.
        let extra_rounds = usize::from(options.output_schema.is_some())
            + usize::from(options.max_tool_calls.is_some())
            + 1;
        let mut rounds = 0usize;
        let mut reminders = runtime_reminders::RunReminders::default();
        loop {
            let round = rounds;
            rounds = rounds.saturating_add(1);
            let refresh = dispatcher.take_tools_refresh();
            if native || refresh || round == 0 {
                tools = self.refresh_run_tools(&run.session_id, options, &tools)?;
                dispatcher = dispatcher.with_tools(tools.clone());
            }
            let budget_final_round = max_rounds.is_some_and(|limit| round >= limit)
                && dispatcher.remaining_tool_calls() == Some(0)
                && !budget_final_requested;
            let enforcing = reminders.enforcing();
            ensure!(
                max_rounds.is_none_or(|limit| {
                    round < limit.saturating_add(extra_rounds)
                        && (round < limit
                            || result_retry_pending
                            || budget_final_round
                            || enforcing)
                }),
                "Maximum model/tool rounds reached"
            );
            budget_final_requested |= budget_final_round;
            ensure!(!cancellation.is_cancelled(), "Cancelled");
            let mut dispatcher = dispatcher
                .clone()
                .with_auto_approval(self.config.auto_approval.clone(), user_request.clone());
            let mut tools_round = runtime_reminders::ToolsRound::Open;
            let (response, dispatcher) = if let Some(execution) = single_tool {
                (
                    ModelResponse {
                        tool_calls: vec![execution.call.clone()],
                        ..Default::default()
                    },
                    dispatcher.clone(),
                )
            } else if let Some(code) = &options.script {
                (crate::script::response(code), dispatcher.clone())
            } else {
                let (default_model, request_provider) = self.provider_binding();
                tools_round = self.queue_run_reminders(run, options, &dispatcher, &mut reminders);
                for reminder in tools.take_reminders(&run.session_id) {
                    self.context_state()
                        .remind_with_metadata(&run.session_id, "root", reminder);
                }
                let mut history = self.history(&run.session_id)?;
                let request_scope = self.config.observation.as_ref().map(|_| ObservationScope {
                    session_id: run.session_id.clone(),
                    run_id: run.id.clone(),
                    model_request_id: Some(id()),
                    ..Default::default()
                });
                let mut late_steering = false;
                let mut steering_batch = Vec::<Message>::new();
                let mut context_notices = Vec::<String>::new();
                let mut preparation_mode = PrepareMode::Request;
                let mut rejected_frontier = None;
                let prepared = async {
                    loop {
                    let system_prompt = tool_surface.prompt();
                    let mut messages = self.config.context.project_history(&history)?;
                    // The new batch must reach token/window admission intact,
                    // even if the ordinary history projection drops older input.
                    messages.retain(|saved| !steering_batch.iter().any(|steer| steer.id == saved.id));
                    messages.extend(steering_batch.iter().cloned());
                    let preview = self.context_state().preview_reminder_message(&run.session_id, "root", &self.store)?;
                    if let Some(message) = &preview { messages.push(message.clone()); }
                    let request = ModelRequest {
                        model: options.model.clone().unwrap_or_else(|| default_model.clone()),
                        system_prompt,
                        messages,
                        tools: tool_surface.declarations(),
                        max_output_tokens: options.max_output_tokens,
                        ..Default::default()
                    };
                    let mut prepared = tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => Err(anyhow!("Cancelled during context preparation")),
                        result = self.prepare_with_writer(&run.session_id, request, request_provider.clone(), preparation_mode, cancellation.clone()) => result,
                    }?;
                    // Preparation may have made physical auxiliary calls even
                    // when a late batch requires a fresh request projection.
                    if let Some(value) = prepared.usage.take() {
                        usage.get_or_insert_with(Usage::default).add(&value);
                    }
                    if let Some(notice) = prepared.notice.take() {
                        context_notices.push(notice);
                    }
                    if self.apply_tool_surface_change(&run.session_id, options, &mut tools, &mut tool_surface, &mut prepared.request)? {
                        dispatcher = dispatcher.clone().with_tools(tools.clone());
                    }
                    // These are run controls, independent of the host's disposable context view.
                    tool_surface.restore(&mut prepared.request);
                    tools_round.apply(&mut prepared.request);
                    ensure!(!cancellation.is_cancelled(), "Cancelled during context preparation");
                    let admitted_steers = prepared.request.messages.iter().filter(|saved|
                        steering_batch.iter().any(|steer| steer.id == saved.id));
                    ensure!(admitted_steers.eq(steering_batch.iter()),
                        "Context preparation changed newly accepted steering");
                    // Preparation may await auxiliary work. Refresh from raw
                    // canonical history if input arrived meanwhile: appending
                    // after preparation would bypass token admission/calibration.
                    let steers = self.store.consume_steering_messages(&run.session_id)?;
                    if !steers.is_empty() {
                        late_steering = true;
                        steering_batch.extend(steers);
                        history = self.history(&run.session_id)?;
                        continue;
                    }
                    let updated = self.context_state().preview_reminder_message(&run.session_id, "root", &self.store)?;
                    if preview.as_ref().map(|m| &m.content) != updated.as_ref().map(|m| &m.content) {
                        continue;
                    }
                    if let Some(preview) = &preview {
                        prepared.request.messages.retain(|saved| saved.id != preview.id);
                    }
                    prepared.request = match self.build_context_request(prepared.request, &run.session_id, request_scope.as_ref(), request_provider.clone()) {
                        Ok(request) => request,
                        Err(error) => {
                            let Some(budget) = error.downcast_ref::<ContextBudgetExceeded>() else {
                                return Err(error);
                            };
                            let frontier = self.config.context.history_frontier(&run.session_id)?;
                            // One attempt is allowed without an applied view. Every
                            // later recovery must have advanced complete coverage.
                            if rejected_frontier.is_some_and(|previous| previous >= frontier) {
                                return Err(error);
                            }
                            rejected_frontier = Some(frontier);
                            let (tokens, bytes) = match preparation_mode {
                                PrepareMode::Pressure { additional_tokens, additional_bytes } => (additional_tokens, additional_bytes),
                                _ => (0, 0),
                            };
                            preparation_mode = PrepareMode::Pressure {
                                additional_tokens: tokens.max(budget.additional_tokens),
                                additional_bytes: bytes.max(budget.additional_bytes),
                            };
                            if let (Some(observer), Some(scope)) = (&self.config.observation, &request_scope) {
                                observer.emit(scope.clone(), "context_budget_pressure", serde_json::json!({
                                    "additional_tokens": budget.additional_tokens,
                                    "additional_bytes": budget.additional_bytes,
                                    "frontier": frontier,
                                }), None);
                            }
                            history = self.history(&run.session_id)?;
                            continue;
                        }
                    };
                    if !context_notices.is_empty() {
                        prepared.notice = Some(context_notices.join("\n"));
                    }
                    return Ok::<_, anyhow::Error>(prepared);
                    }
                }.await;
                if let (Err(error), Some(observer), Some(scope)) =
                    (&prepared, &self.config.observation, &request_scope)
                {
                    observer.failure(
                        scope.clone(),
                        "model_prepare_failed",
                        0,
                        "preparation_error",
                        &error.to_string(),
                    );
                }
                let prepared = prepared?;
                if late_steering && self.config.auto_approval.is_some() {
                    user_request = self.latest_user_request(&run.session_id)?;
                    dispatcher = dispatcher.with_auto_approval(
                        self.config.auto_approval.clone(),
                        user_request.clone(),
                    );
                }
                if let Some(notice) = prepared.notice {
                    let context = ToolContext {
                        session_id: run.session_id.clone(),
                        run_id: run.id.clone(),
                        invocation_id: String::new(),
                        cancellation: cancellation.clone(),
                        approval: dispatcher.approval_policy(&run.session_id),
                    };
                    dispatcher.emit_output(
                        &context,
                        &format!("context:{}", id()),
                        serde_json::json!({"type":"text","text":notice}),
                    )?;
                    ensure!(
                        !cancellation.is_cancelled(),
                        "Cancelled before context notice"
                    );
                    self.emit(
                        &run.session_id,
                        &run.id,
                        EventKind::Paragraph { text: notice },
                    );
                }
                ensure!(
                    !cancellation.is_cancelled(),
                    "Cancelled before provider dispatch"
                );
                let mut request = prepared.request;
                // Provider extension history cursor (docs/providers.md).
                request.session_id = run.session_id.clone();
                request.history = self.cursors.advance(&run.session_id, &request.messages);
                let (provider_name, model_name) = request
                    .model
                    .split_once('/')
                    .unwrap_or(("unknown", request.model.as_str()));
                let mut fallback_usage = RequestUsage::new(provider_name, model_name);
                fallback_usage.session_id.clone_from(&run.session_id);
                fallback_usage.run_id = Some(run.id.clone());
                if let Some(request_id) = request_scope
                    .as_ref()
                    .and_then(|scope| scope.model_request_id.as_ref())
                {
                    fallback_usage.request_id.clone_from(request_id);
                }
                let request_started = Instant::now();
                let first_text = self
                    .config
                    .observation
                    .as_ref()
                    .map(|_| Arc::new(Mutex::new(None::<u64>)));
                self.observe_model_start(request_scope.as_ref(), &request, &request_provider);
                let dispatcher = dispatcher.with_request(
                    request_scope
                        .as_ref()
                        .and_then(|s| s.model_request_id.clone()),
                );
                let paragraphs = Arc::new(Mutex::new(Paragraphs::default()));
                let buffer = paragraphs.clone();
                // A provider retaining a callback must not retain the archive or engine.
                let log = self.state.log.clone();
                let session = run.session_id.clone();
                let run_id = run.id.clone();
                let max = self.config.max_paragraph_bytes.max(4);
                let callback_live = Arc::new(AtomicBool::new(true));
                let callback_lifetime = CallbackLifetime(callback_live.clone());
                let callback_cancellation = cancellation.clone();
                let callback_first_text = first_text.clone();
                let callback: TextCallback = Arc::new(move |text| {
                    if !callback_live.load(Ordering::Relaxed)
                        || callback_cancellation.is_cancelled()
                    {
                        return;
                    }
                    if !text.is_empty()
                        && let Some(first) = &callback_first_text
                    {
                        first
                            .lock()
                            .unwrap()
                            .get_or_insert(request_started.elapsed().as_millis() as u64);
                    }
                    if !text.is_empty() {
                        log.progress(&session, &run_id, text.to_owned());
                    }
                    let mut buffer = buffer.lock().unwrap();
                    for piece in buffer.push_bounded(text, max) {
                        log.paragraph(&session, &run_id, piece);
                    }
                });
                self.state.log.transient(
                    &run.session_id,
                    &run.id,
                    EventKind::ModelRequestStarted {},
                );
                self.config.context.observe_request(
                    &run.session_id,
                    &request,
                    request_provider.clone(),
                );
                let (mut response_result, mut captured_usage) = capture_request_usage_with_store(
                    &self.store, &run.session_id, Some(&run.id), RequestKind::Main,
                    async { tokio::select! {biased; _=cancellation.cancelled()=>Err(anyhow!("Cancelled during provider request")), result=request_provider.generate(request,callback,cancellation.clone())=>result} },
                ).await;
                if captured_usage.is_empty() {
                    match &response_result {
                        Ok(response) => {
                            fallback_usage.usage = response.usage.clone().unwrap_or_default();
                            if let Some((provider, model)) = response
                                .provider_state
                                .as_ref()
                                .and_then(|state| state.get("_bubblepaw_selected_model"))
                                .and_then(serde_json::Value::as_str)
                                .and_then(|selected| selected.split_once('/'))
                            {
                                fallback_usage.provider = provider.to_owned();
                                fallback_usage.account = provider.to_owned();
                                fallback_usage.model = model.to_owned();
                            }
                        }
                        Err(error) => {
                            fallback_usage.failed = true;
                            fallback_usage.usage = crate::ProviderError::of(error)
                                .and_then(|error| error.usage.clone())
                                .unwrap_or_default();
                        }
                    }
                    captured_usage.push(fallback_usage);
                }
                drop(callback_lifetime);
                self.observe_model_result(
                    &run.session_id,
                    &captured_usage,
                    request_scope.as_ref(),
                    &mut response_result,
                    (
                        request_started.elapsed().as_millis() as u64,
                        first_text.as_ref().and_then(|v| *v.lock().unwrap()),
                    ),
                    cancellation.is_cancelled(),
                );
                request_usage.extend(captured_usage);
                let response = match response_result {
                    // The select above only sees cancellation it polls after; a reply
                    // that resolves after cancel was signalled is not a successful turn.
                    Ok(response) if cancellation.is_cancelled() => {
                        if let Some(value) = &response.usage {
                            usage.get_or_insert_with(Usage::default).add(value);
                        }
                        return Err(anyhow!("Cancelled during provider request"));
                    }
                    Ok(response) => response,
                    Err(error) => {
                        if let Some(value) =
                            crate::ProviderError::of(&error).and_then(|error| error.usage.as_ref())
                        {
                            usage.get_or_insert_with(Usage::default).add(value);
                        }
                        return Err(error);
                    }
                };
                result_retry_pending = false;
                reminders.observe_tool_calls(tools_round, response.tool_calls.len());
                self.config
                    .context
                    .observe_response(&run.session_id, &response);
                {
                    let mut buffer = paragraphs.lock().unwrap();
                    let streamed = buffer.had_text();
                    let pieces = if streamed {
                        buffer.finish()
                    } else {
                        buffer
                            .push_bounded(&response.text, max)
                            .into_iter()
                            .chain(buffer.finish())
                            .collect()
                    };
                    for piece in pieces {
                        self.emit(
                            &run.session_id,
                            &run.id,
                            EventKind::Paragraph { text: piece },
                        );
                    }
                }
                if let Some(value) = &response.usage {
                    usage.get_or_insert_with(Usage::default).add(value);
                }
                (response, dispatcher)
            };
            // Reissuing a durable call ID is never a new invocation, including after
            // an unknown outcome. The host must resolve that effect explicitly.
            let mut call_ids = std::collections::HashSet::new();
            for call in &response.tool_calls {
                ensure!(
                    !call.id.is_empty()
                        && call_ids.insert(call.id.clone())
                        && !self.store.has_tool_call(&run.session_id, &call.id)?,
                    "Provider returned duplicate, reused, or empty tool call IDs"
                );
            }
            ensure!(
                serde_json::to_vec(&response)?.len() <= self.config.max_request_bytes,
                "Provider response exceeds configured storage size limit"
            );
            let mut message =
                Store::message(&run.session_id, Role::Assistant, response.text.clone());
            message.tool_calls = response.tool_calls.clone();
            message.provider_state = response.provider_state;
            // The ordinary final response, result pointer, run state and its two
            // events are one commit in drive. Tool rounds still checkpoint
            // messages before crossing any dispatch/approval boundary.
            if options.script.is_none()
                && response.tool_calls.is_empty()
                && options.output_schema.is_some()
            {
                user_request = self
                    .nudge_required_result(run, &message, request_usage, &mut result_nudged)?
                    .unwrap_or(user_request);
                result_retry_pending = true;
                continue;
            }
            if options.script.is_none()
                && response.tool_calls.is_empty()
                && !self
                    .store
                    .queued_inputs(&run.session_id)?
                    .iter()
                    .any(|input| input.state == "pending" && input.kind == crate::InputKind::Steer)
            {
                return Ok(runtime_result::text_outcome(message, true));
            }
            // The dispatcher admits every route. A sole Allow call can still
            // batch its assistant reply and dispatch marker in one transaction.
            let make_context = || ToolContext {
                session_id: run.session_id.clone(),
                run_id: run.id.clone(),
                invocation_id: String::new(),
                cancellation: cancellation.clone(),
                approval: Default::default(),
            };
            let mut single_prepared = if response.tool_calls.len() == 1 {
                Some(dispatcher.prepare_call(
                    make_context(),
                    response.tool_calls[0].clone(),
                    tools_round.is_open(),
                )?)
            } else {
                None
            };
            let single_allowed = if options.script.is_none() {
                match &mut single_prepared {
                    Some(prepared) => dispatcher.admit_fast_path(prepared)?,
                    None => false,
                }
            } else {
                false
            };
            let mut prepared_invocation = if single_allowed {
                Some(dispatcher.commit_reply_dispatch(
                    &message,
                    request_usage,
                    single_prepared.as_ref().expect("single prepared call"),
                )?)
            } else {
                self.state.log.settle_text(&run.session_id, || {
                    self.store
                        .append_with_request_usage(&message, request_usage)
                })?;
                None
            };
            request_usage.clear();
            // Native submissions settle last. Calls retain their provider order
            // in history; unrelated effects run before accepting the result.
            let ordered_calls = response
                .tool_calls
                .iter()
                .filter(|call| !is_submission(call))
                .chain(
                    response
                        .tool_calls
                        .iter()
                        .filter(|call| is_submission(call)),
                );
            for call in ordered_calls {
                let prepared = match single_prepared.take() {
                    Some(prepared) => prepared,
                    None => dispatcher.prepare_call(
                        make_context(),
                        call.clone(),
                        tools_round.is_open(),
                    )?,
                };
                let dispatched = dispatcher
                    .invoke_direct(prepared, &message.id, prepared_invocation.take())
                    .await?;
                let result = dispatched.result;
                let footer = dispatcher
                    .remaining_tool_calls()
                    .map(|remaining| format!("\n\n{remaining} tool calls left"))
                    .unwrap_or_default();
                let result = ToolResult {
                    content: format!(
                        "{}{}",
                        bounded(
                            &result.content,
                            self.config
                                .max_tool_result_bytes
                                .saturating_sub(footer.len())
                        ),
                        footer
                    ),
                    ..result
                };
                let mut result_message =
                    Store::message(&run.session_id, Role::Tool, result.content.clone());
                let captured_result = single_tool.map(|_| result.clone());
                result_message.tool_call_id = Some(call.id.clone());
                result_message.is_error = result.is_error;
                dispatcher.record_direct_result(
                    &run.id,
                    &result_message,
                    call,
                    result,
                    dispatched.invocation.as_deref(),
                )?;
                if let (Some(execution), Some(result)) = (single_tool, captured_result) {
                    *execution.result.lock().unwrap() = Some(result);
                }
            }
            if let Some(value) = dispatcher.take_submitted_result() {
                return runtime_result::outcome(run, value);
            }
            if options.script.is_some() || single_tool.is_some() {
                if single_tool.is_none() {
                    self.store.consume_generated_steering(&run.session_id)?;
                }
                let history = self.history(&run.session_id)?;
                let result_message_id = history
                    .iter()
                    .rev()
                    .find(|message| {
                        message.tool_call_id.as_deref() == Some(response.tool_calls[0].id.as_str())
                    })
                    .map(|message| message.id.clone())
                    .ok_or_else(|| anyhow!("Script result was not recorded"))?;
                return crate::script::outcome(&history, &response.tool_calls).map(|text| {
                    TurnOutcome {
                        text,
                        result_message_id,
                        final_message: None,
                    }
                });
            }
            // Consume steering after a complete model/tool round. The final
            // preparation drain also catches admissions arriving after this.
            let steered = self.store.consume(&run.session_id, true, false)?;
            if steered > 0 && self.config.auto_approval.is_some() {
                user_request = self.latest_user_request(&run.session_id)?;
            }
            if options.output_schema.is_none() && response.tool_calls.is_empty() && steered == 0 {
                return Ok(runtime_result::text_outcome(message, false));
            }
        }
    }
}
pub(crate) fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.into();
    }
    const MARKER: &str = "\n[tool result truncated]";
    let room = max.saturating_sub(MARKER.len());
    let mut end = room.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if max < MARKER.len() {
        return MARKER[..max].into();
    }
    format!("{}{}", &text[..end], MARKER)
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
