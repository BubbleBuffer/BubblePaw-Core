use crate::journal::{Consumed, InputScope, Journal, RunFinish};
use crate::request_seam::{Built, Draft, Note, Release, RequestContext};
use crate::{
    CancellationToken, ClientInfo, EventKind, HarnessEvent, InputKind, Message, MessageInput,
    ModelRequest, ModelResponse, Observation, ObservationScope, Provider, QueuedInput, Role, Run,
    RunOptions, Store, ToolCall, ToolContext, ToolDispatcher, ToolRegistry, ToolResult, Usage, id,
    now, stream::Paragraphs,
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
#[path = "runtime_overflow.rs"]
mod runtime_overflow;
#[path = "runtime_preparation.rs"]
mod runtime_preparation;
pub use runtime_preparation::{
    ContextBudgetExceeded, ContextPreparation, PrepareCall, PrepareMode, PreparedContext,
};
#[path = "runtime_input.rs"]
mod runtime_input;
#[path = "runtime_reminders.rs"]
mod runtime_reminders;
#[path = "runtime_result.rs"]
mod runtime_result;
#[path = "runtime_stream.rs"]
mod runtime_stream;
#[path = "runtime_supervision.rs"]
mod runtime_supervision;
#[path = "runtime_tools.rs"]
mod runtime_tools;
use crate::submit_result::is_submission;

#[path = "runtime_records.rs"]
mod records;
#[path = "run_loop.rs"]
mod run_loop;
#[path = "run_settle.rs"]
mod run_settle;
pub(crate) use records::Active;
use records::{CallbackLifetime, SingleToolRun, TurnOutcome};
use run_loop::{RunInputs, RunOutcome};

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
    /// Baseline approval policy for tool calls.
    pub approval: crate::ApprovalPolicy,
    /// Run approval restrictions are combined with this policy at each tool call.
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
#[derive(Default)]
pub(crate) struct RuntimeState {
    active: Mutex<HashMap<String, Active>>,
    compacting: Mutex<HashMap<String, CancellationToken>>,
    finalized: std::sync::Condvar,
    stopping: AtomicBool,
    pub(crate) approvals: crate::approvals::Waiters,
    /// One event sequence and broadcast per store (docs/session-api.md).
    pub(crate) log: Arc<crate::event_log::Log>,
    histories: Mutex<HashMap<String, runtime_context::ParsedHistory>>,
    /// The one prompt-cache ledger of this store; its `context` is the root lineage.
    pub(crate) cache: Arc<crate::CacheAccounting>,
    pub(crate) session_cleanups: Mutex<HashMap<String, crate::store::SessionCleanup>>,
    /// Request usage the runs have made and no commit has written yet.
    pub(crate) usage: crate::request_metering::UsageLedger,
}
/// Existing inventory body header, retained as frozen text. Reminder identity
/// and restart deduplication use structured runtime_inventory metadata.
pub const RUNTIME_INVENTORY_HEADER: &str = "Current volatile runtime facts";

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
            *history = runtime_context::ParsedHistory::default();
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
        // A finalizing run has already published SessionSettled; like admission,
        // wait for its record instead of refusing a client that saw it settle.
        let active = self.admission_guard(session_id);
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
    selection: runtime_mode::ToolSelection,
    config: HarnessConfig,
    state: Arc<RuntimeState>,
    cursors: Arc<crate::HistoryTracker>,
    journal: Arc<dyn Journal>,
    live: Arc<dyn crate::live_events::LiveEvents>,
    request_context: Arc<dyn RequestContext>,
}
impl Harness {
    pub fn new(
        store: Store,
        provider: Arc<dyn Provider>,
        tools: ToolRegistry,
        config: HarnessConfig,
    ) -> Self {
        let state = store.runtime();
        state.log.init(config.event_capacity);
        let journal = Arc::new(crate::journal_sqlite::SqliteJournal::new(
            store.clone(),
            state.log.clone(),
        ));
        let live: Arc<dyn crate::live_events::LiveEvents> = state.log.clone();
        let selection = runtime_mode::ToolSelection::new(
            store.clone(),
            tools,
            config.registry_resolver.clone(),
            config.execution_mode,
        );
        let request_context = Arc::new(runtime_context::StockRequestContext {
            hook: config.context.clone(),
            store: store.clone(),
            state: state.clone(),
            selection: selection.clone(),
            observation: config.observation.clone(),
            max_request_bytes: config.max_request_bytes,
        });
        let harness = Self {
            store,
            provider,
            selection,
            config,
            state,
            cursors: Arc::default(),
            journal,
            live,
            request_context,
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
    /// Events of every harness on this store, nested agents included.
    pub fn subscribe(&self) -> broadcast::Receiver<HarnessEvent> {
        self.state.log.subscribe()
    }
    #[cfg(test)]
    pub(crate) fn emit(&self, session_id: &str, run_id: &str, kind: EventKind) {
        self.state
            .log
            .emit(&self.store, session_id, run_id, kind, None);
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
        text: impl Into<MessageInput>,
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
        text: impl Into<MessageInput>,
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
        text: impl Into<MessageInput>,
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
                    && run.error.as_deref() == Some(result.body().as_ref())),
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
        self.cursors.forget(session_id);
        self.request_context.release(session_id, Release::Deleted);
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
        self.cursors.forget(session_id);
        self.request_context.release(session_id, Release::Idle);
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
        // then release admission before hooks recurse into nested cancellation.
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
            // What the host keeps for the run is let go with the iteration, however it ends.
            let _hold = self.request_context.run_hold(&run);
            let RunOutcome {
                status,
                text,
                usage,
                error,
                result_message_id,
                final_message,
            } = self
                .run_once(&RunInputs {
                    run: &run,
                    options: &options,
                    cancellation: &cancellation,
                    single_tool: single_tool.as_ref(),
                })
                .await;
            run.status = status;
            run.text = text;
            run.usage = usage;
            run.error = error;
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
                && self
                    .journal
                    .input_pending(&run.session_id, InputScope::Any)
                    .unwrap_or(false);
            let checkpoint = self.journal.run_finished(RunFinish {
                run: &run,
                result_message_id: result_message_id.as_deref(),
                final_message: final_message.as_ref(),
                origin,
                settles_session: !continue_run,
            });
            if let Err(error) = &checkpoint {
                run.status = "failed".into();
                run.error = Some(format!("Run checkpoint failed: {error}"));
            }
            if continue_run && checkpoint.is_ok() {
                // Input queued during a script run is answered by a model run.
                options.script = None;
                let next = self.new_run(&run.session_id, &options);
                if self.journal.run_continued(&next).is_ok() {
                    if let Some(value) = active.get_mut(&run.session_id) {
                        value.run_id = next.id.clone();
                    }
                    run = next;
                    drop(active);
                    continue;
                }
                self.journal.session_settled(&run.session_id, &run.id);
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
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
