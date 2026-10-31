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

#[path = "agents_question.rs"]
mod questions;
#[path = "agent_reminders.rs"]
mod reminder_notices;
mod worktree_cleanup;
use worktree_cleanup::report_worktree_cleanup;

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
impl AgentTools {
    pub fn new(store: Store, read_only_child: Harness, workspace_child: Option<Harness>) -> Self {
        if let Err(error) = crate::rewind::install_checkpoint_sweep(&store) {
            tracing::warn!(%error, "rewind checkpoint sweep is not installed");
        }
        let mut profiles = vec![AgentProfile {
            name: "read-only".into(),
            description: "Read-only workspace child".into(),
            max_rounds: read_only_child.max_rounds(),
            harness: Arc::new(read_only_child),
            model: None,
            approval: None,
        }];
        if let Some(child) = workspace_child {
            profiles.push(AgentProfile {
                name: "workspace".into(),
                description: "Writable workspace child".into(),
                max_rounds: child.max_rounds(),
                harness: Arc::new(child),
                model: None,
                approval: None,
            });
        }
        Self::from_profiles(store, profiles).expect("valid built-in profiles")
    }
    pub fn from_profiles(store: Store, profiles: Vec<AgentProfile>) -> Result<Self> {
        validate_profiles(&profiles)?;
        Ok(Self {
            store,
            profiles: OnceLock::from(profiles),
            repl: None,
            processes: None,
            registry: Arc::default(),
            _nested: Vec::new(),
            cancel_on_drop: true,
        })
    }
    /// One shared manager for every depth. `build` receives the Arc before
    /// profiles exist and must pass it as `nested`, so each child harness holds
    /// only a weak handle back here. Unlimited depth therefore creates no
    /// strong ownership cycle and no finite depth ceiling.
    pub fn new_recursive<F>(
        store: Store,
        limits: Agents,
        registry: Arc<AgentRegistry>,
        repl: Option<Arc<CodeMode>>,
        processes: Option<Arc<ProcessTools>>,
        build: F,
    ) -> Result<Arc<Self>>
    where
        F: FnOnce(&Arc<Self>) -> Result<Vec<AgentProfile>>,
    {
        if let Err(error) = crate::rewind::install_checkpoint_sweep(&store) {
            tracing::warn!(%error, "rewind checkpoint sweep is not installed");
        }
        registry.wake.configure(
            limits.auto_wake,
            Duration::from_millis(limits.wake_debounce_ms),
        );
        if let Some(processes) = &processes {
            processes.set_wake(registry.wake.clone());
            processes.set_store(store.clone());
        }
        registry.limits.set(limits);
        let manager = Arc::new(Self {
            store,
            profiles: OnceLock::new(),
            repl,
            processes,
            registry,
            _nested: Vec::new(),
            cancel_on_drop: true,
        });
        let profiles = build(&manager)?;
        validate_profiles(&profiles)?;
        manager
            .profiles
            .set(profiles)
            .map_err(|_| anyhow!("agent profiles are already initialized"))?;
        Ok(manager)
    }
    fn profiles(&self) -> &[AgentProfile] {
        match self.profiles.get() {
            Some(profiles) => profiles.as_slice(),
            None => &[],
        }
    }
    /// All managers and wake adapters on this host share the latest limits.
    /// Running child turn controls are intentionally kept in their own snapshot.
    pub fn apply_limits(&self, limits: &Agents) {
        self.registry.limits.set(limits.clone());
        self.registry.wake.configure(
            limits.auto_wake,
            Duration::from_millis(limits.wake_debounce_ms),
        );
    }
    fn limits(&self) -> Agents {
        self.registry.limits.get()
    }
    /// Follow the host's published agents section. Limits are read from it at
    /// use; `reconcile` carries the wake settings, which are held in atomics.
    pub fn follow(&self, section: tokio::sync::watch::Receiver<Arc<Agents>>) {
        self.registry.limits.follow(section);
        self.reconcile();
    }
    /// Apply the followed section's wake settings to the coordinator.
    pub fn reconcile(&self) {
        let limits = self.limits();
        self.registry.wake.configure(
            limits.auto_wake,
            Duration::from_millis(limits.wake_debounce_ms),
        );
    }
    pub fn with_limits(self, limits: Agents) -> Self {
        self.apply_limits(&limits);
        self
    }
    pub fn with_registry(mut self, registry: Arc<AgentRegistry>) -> Self {
        let configured = self.registry.limits.fixed();
        self.registry = registry;
        if let Some(limits) = configured {
            self.apply_limits(&limits);
        }
        self
    }
    /// Resolve a child call from its originating parent policy, then clamp the
    /// fixed profile restriction. Weak registry ownership avoids a harness cycle.
    pub(crate) fn configure_child_approval(&self, config: &mut HarnessConfig) {
        self.registry.live_approval.store(true, Ordering::Release);
        let registry = Arc::downgrade(&self.registry);
        let fallback = config.approval_resolver.clone();
        let policy = config.approval.clone();
        let store = self.store.clone();
        config.approval_resolver = Some(Arc::new(move |session| {
            let source = registry
                .upgrade()
                .and_then(|registry| registry.approval_sources.lock().ok()?.get(session).cloned());
            if let Some(source) = source {
                return source(session);
            }
            let current = fallback
                .as_ref()
                .map_or_else(|| policy.clone(), |resolve| resolve(session));
            let restriction = store
                .attachment(session, BINDING_ID)
                .ok()
                .flatten()
                .and_then(|(metadata, _)| serde_json::from_str::<Binding>(&metadata).ok())
                .and_then(|binding| binding.approval);
            clamp_approval(&current, restriction.as_ref())
        }));
    }
    fn live_child_approval(&self, harness: &Arc<Harness>) -> bool {
        self.registry.live_approval.load(Ordering::Acquire)
            && self
                .profiles()
                .iter()
                .any(|profile| Arc::ptr_eq(&profile.harness, harness))
    }
    #[allow(clippy::too_many_arguments)]
    fn child_run_options(
        &self,
        session: &str,
        harness: &Arc<Harness>,
        binding: &Binding,
        approval: ApprovalPolicy,
        controls: &Controls,
        context: &ToolContext,
        dispatcher: Option<&ToolDispatcher>,
    ) -> RunOptions {
        let mut options = run_options(binding, approval, controls);
        if self.live_child_approval(harness)
            || (self.registry.live_approval.load(Ordering::Acquire)
                && self.registry.workspace_host.get().is_some()
                && !binding.worker)
        {
            let source = match dispatcher {
                Some(dispatcher) => dispatcher.approval_resolver(&context.session_id),
                None => {
                    let policy = context.approval.clone();
                    Arc::new(move |_: &str| policy.clone()) as ApprovalResolver
                }
            };
            let restriction = binding.approval.clone();
            let source: ApprovalResolver =
                Arc::new(move |session| clamp_approval(&source(session), restriction.as_ref()));
            self.registry
                .approval_sources
                .lock()
                .expect("child approval sources lock")
                .insert(session.to_owned(), source);
            options.approval = None;
        }
        options
    }
    pub fn with_nested(mut self, nested: Vec<Arc<AgentTools>>) -> Self {
        self._nested = nested;
        self
    }
    pub fn with_repl(mut self, repl: Arc<CodeMode>) -> Self {
        self.repl = Some(repl);
        self
    }
    pub fn with_processes(mut self, processes: Arc<ProcessTools>) -> Self {
        processes.set_wake(self.registry.wake.clone());
        processes.set_store(self.store.clone());
        self.processes = Some(processes);
        self
    }
    pub fn wake_coordinator(&self) -> Arc<WakeCoordinator> {
        self.registry.wake.clone()
    }
    pub(crate) fn set_workspace_host(&self, host: WorkspaceHost) -> Result<()> {
        self.registry
            .workspace_host
            .set(host)
            .map_err(|_| anyhow!("child workspace host already configured"))
    }
    fn workspace_runtime(&self, session: &str, profile: &str) -> Result<Option<Arc<ChildRuntime>>> {
        let Some(host) = self.registry.workspace_host.get() else {
            return Ok(None);
        };
        let workspace = crate::worktrees::workspace(&self.store, session, &host.root)?;
        if workspace == host.root {
            return Ok(None);
        }
        Ok(Some(Arc::new((host.factory)(&workspace, profile)?)))
    }
    fn admit_scoped<T>(&self, session: &str, start: impl FnOnce() -> Result<T>) -> Result<T> {
        match &self.registry.scope {
            Some(scope) => scope.admit_owned(&self.store, session, start),
            None => start(),
        }
    }
    fn register_child_wake(&self, child: &Arc<Child>) {
        // Hidden workers are silent outward, but still receive their own
        // ordinary children's completions through the same lifecycle.
        let hidden = child.hidden;
        let weak_child = Arc::downgrade(child);
        let owner = child.owner.clone();
        let session = child.session_id.clone();
        let weak_harness = Arc::downgrade(&child.harness);
        // Worktree adapters have no shared root profile to retain their harness.
        // Retain them with the wake registration so eviction still permits wake.
        let runtime = child.runtime.clone();
        let binding = child.binding.clone();
        let (approval, schema) = {
            let turn = child.turn.lock().expect("child turn lock");
            (turn.approval.clone(), turn.controls.output_schema.clone())
        };
        let depth = child.depth;
        let weak_registry = Arc::downgrade(&self.registry);
        let weak_repl = self.repl.as_ref().map(Arc::downgrade);
        let weak_processes = self.processes.as_ref().map(Arc::downgrade);
        let store = self.store.clone();
        let registration = self.registry.wake.register_reminders(
            child.session_id.clone(),
            Arc::new(move |reminders| {
                let registry = weak_registry
                    .upgrade()
                    .ok_or_else(|| anyhow!("agent host was released"))?;
                // The target survives handle eviction through a catalog identity,
                // with weak profile/lifecycle links to avoid ownership cycles.
                let manager = AgentTools {
                    store: store.clone(),
                    profiles: OnceLock::from(vec![AgentProfile {
                        name: binding.profile.clone(),
                        description: String::new(),
                        model: binding.model.clone(),
                        max_rounds: binding.max_rounds,
                        approval: binding.approval.clone(),
                        harness: weak_harness
                            .upgrade()
                            .ok_or_else(|| anyhow!("child profile host was released"))?,
                    }]),
                    repl: weak_repl.as_ref().and_then(Weak::upgrade),
                    processes: weak_processes.as_ref().and_then(Weak::upgrade),
                    registry,
                    _nested: Vec::new(),
                    cancel_on_drop: false,
                };
                let child = match weak_child.upgrade() {
                    Some(child) => child,
                    None => {
                        // Preserve the host lifecycle identity without opening the
                        // chat: canonical history is loaded only after guarded
                        // start_if_active admission. An archive stays closed.
                        let at = Instant::now();
                        Arc::new(Child {
                            owner: owner.clone(),
                            session_id: session.clone(),
                            handle: format!("agent:{}", uuid::Uuid::new_v4()),
                            created: at,
                            store: store.clone(),
                            harness: weak_harness
                                .upgrade()
                                .ok_or_else(|| anyhow!("child profile host was released"))?,
                            runtime: runtime.clone(),
                            binding: binding.clone(),
                            original_profile: true,
                            hidden,
                            depth,
                            wake: Arc::downgrade(&manager.registry.wake),
                            changed: Notify::new(),
                            turn: Mutex::new(Turn {
                                result: Some(json!({"status":"idle"})),
                                unread: false,
                                run_id: None,
                                stop: CancellationToken::new(),
                                cancelling: false,
                                controls: Controls {
                                    output_schema: schema.clone(),
                                    ..Default::default()
                                },
                                approval: approval.clone(),
                                control_error: None,
                                started: at,
                                last_interaction: at,
                                last_reminder: None,
                                active_waits: 0,
                            }),
                        })
                    }
                };
                manager.start_wake_child(child, reminders, approval.clone())
            }),
        );
        let previous = self
            .registry
            .state
            .lock()
            .expect("agent registry lock")
            .wake_registrations
            .insert(child.session_id.clone(), registration);
        drop(previous);
    }
    fn start_wake_child(
        &self,
        child: Arc<Child>,
        reminders: Vec<bubblepaw_core::Reminder>,
        approval: ApprovalPolicy,
    ) -> Result<Run> {
        self.start_wake_child_input(child, reminders, approval, false)
    }
    fn start_wake_child_input(
        &self,
        mut child: Arc<Child>,
        mut reminders: Vec<bubblepaw_core::Reminder>,
        approval: ApprovalPolicy,
        resume_queued: bool,
    ) -> Result<Run> {
        let workspace_guard = crate::worktrees::admission_guard()?;
        let mut state = self
            .registry
            .state
            .lock()
            .map_err(|_| anyhow!("agent registry poisoned"))?;
        if let Some(retained) = state
            .children
            .values()
            .find(|existing| existing.session_id == child.session_id)
        {
            child = retained.clone();
        }
        ensure!(!state.stopping, "child cannot wake");
        {
            let turn = child
                .turn
                .lock()
                .map_err(|_| anyhow!("agent result poisoned"))?;
            ensure!(
                !turn.cancelling,
                "child cancellation cleanup is in progress"
            );
            if turn.result.is_none() {
                return Err(bubblepaw_core::SessionBusy {
                    session_id: child.session_id.clone(),
                    run_id: turn.run_id.clone().unwrap_or_default(),
                }
                .into());
            }
        }
        self.check_slots(&state, &child.owner)?;
        let retain = !state
            .children
            .values()
            .any(|existing| existing.session_id == child.session_id);
        if retain {
            self.retain_slot(&mut state)?;
        }
        let mut turn = child
            .turn
            .lock()
            .map_err(|_| anyhow!("agent result poisoned"))?;
        let controls = Controls {
            output_schema: turn.controls.output_schema.clone(),
            ..Default::default()
        };
        if turn.controls.max_tool_calls.is_some() {
            reminders.push(bubblepaw_core::Reminder::new("child_budget",
                "This turn has an unlimited tool-call budget; the previous turn's budget does not carry over."));
        }
        turn.approval = approval.clone();
        let mut options = run_options(&child.binding, approval.clone(), &controls);
        if self.live_child_approval(&child.harness) || child.runtime.is_some() {
            options.approval = None;
        }
        options.origin = MessageOrigin::Generated;
        let events = child.harness.subscribe();
        let started = Instant::now();
        let run = self.admit_scoped(&child.session_id, || {
            if resume_queued {
                child.harness.start_resume(&child.session_id, options)
            } else {
                child
                    .harness
                    .start_reminders_if_active(&child.session_id, reminders, options)
            }
        })?;
        if retain {
            state.children.insert(child.handle.clone(), child.clone());
        }
        let control_error = save_controls(&self.store, &child.session_id, &run.id, &controls)
            .err()
            .map(|e| e.to_string());
        turn.result = None;
        turn.unread = false;
        turn.run_id = Some(run.id.clone());
        turn.stop = CancellationToken::new();
        if control_error.is_some() {
            turn.stop.cancel();
        }
        turn.controls = controls;
        turn.control_error = control_error;
        turn.started = started;
        turn.last_interaction = started;
        turn.last_reminder = None;
        drop(turn);
        drop(state);
        drop(workspace_guard);
        let context = ToolContext {
            session_id: child.owner.clone(),
            run_id: String::new(),
            invocation_id: String::new(),
            cancellation: CancellationToken::new(),
            approval,
        };
        self.observe_turn(child, &context, None, events);
        Ok(run)
    }
    pub fn profile(&self, name: &str) -> Option<&AgentProfile> {
        self.profile_enabled(name)
            .then(|| self.stored_profile(name))
            .flatten()
    }
    fn stored_profile(&self, name: &str) -> Option<&AgentProfile> {
        self.profiles().iter().find(|p| p.name == name)
    }
    fn profile_enabled(&self, name: &str) -> bool {
        self.registry
            .profile_availability
            .get()
            .is_none_or(|enabled| enabled(name))
    }
    pub(crate) fn set_profile_availability(&self, enabled: ProfileAvailability) {
        let _ = self.registry.profile_availability.set(enabled);
    }
    pub fn register(self: &Arc<Self>, registry: &mut ToolRegistry) -> Result<()> {
        self.register_tools(registry, Manager::Strong(self.clone()))
    }
    /// Nested harness capabilities hold weak managers, avoiding ownership cycles.
    pub fn register_weak(self: &Arc<Self>, registry: &mut ToolRegistry) -> Result<()> {
        self.register_tools(registry, Manager::Weak(Arc::downgrade(self)))
    }
    fn register_tools(&self, registry: &mut ToolRegistry, manager: Manager) -> Result<()> {
        // Fresh registrations capture the current profile enum and limit text;
        // existing registry snapshots retain their original definition.
        for name in ["agent", "agent_send", "agent_status"] {
            registry.register_host(Arc::new(AgentTool {
                manager: manager.clone(),
                name,
                definition: OnceLock::new(),
            }))?;
        }
        Ok(())
    }
    pub fn continuation_status(&self, owner: &str) -> Value {
        let Ok(state) = self.registry.state.lock() else {
            return json!({"available":false});
        };
        // Listed while running, or while a finished result is unread. Consumed results are omitted.
        let mut entries = state.children.values().filter(|c| c.owner == owner && !c.hidden).filter_map(|c| {
            let turn = c.turn.lock().ok();
            let quota = c.store.quota_wait(&c.session_id).ok().flatten();
            let running = quota.is_some() || turn.as_ref().is_none_or(|t| t.result.is_none());
            let unread = turn.as_ref().is_some_and(|t| t.unread);
            if !running && !unread {
                return None;
            }
            let status = if quota.is_some() { "waiting_for_quota" } else { turn.as_ref().and_then(|t| t.result.as_ref()).and_then(|v| v["status"].as_str()).unwrap_or("running") };
            Some((running,c.created,json!({"handle":c.handle,"sessionId":c.session_id,"status":status,"resultAvailable":!running,"unread":unread})))
        }).collect::<Vec<_>>();
        entries.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        let omitted = entries.len().saturating_sub(4);
        json!({"available":true,"entries":entries.into_iter().take(4).map(|(_,_,v)| v).collect::<Vec<_>>(),"omitted":omitted})
    }
    pub fn start(&self, context: &ToolContext, input: Value) -> Result<Value> {
        self.start_owned(context, input, None)
    }
    /// Host adapter launch preserving the originating run's remaining tool budget.
    pub fn start_with_dispatcher(
        &self,
        context: &ToolContext,
        input: Value,
        dispatcher: ToolDispatcher,
    ) -> Result<Value> {
        self.start_owned(context, input, Some(dispatcher))
    }
    fn inherited_controls(
        &self,
        context: &ToolContext,
        dispatcher: Option<&ToolDispatcher>,
        mut controls: Controls,
    ) -> Result<Controls> {
        if let Some(dispatcher) = dispatcher {
            controls.max_tool_calls =
                minimum(controls.max_tool_calls, dispatcher.remaining_tool_calls());
        }
        let state = self
            .registry
            .state
            .lock()
            .map_err(|_| anyhow!("agent registry poisoned"))?;
        if let Some(parent) = state
            .children
            .values()
            .find(|child| child.session_id == context.session_id)
        {
            let turn = parent
                .turn
                .lock()
                .map_err(|_| anyhow!("agent result poisoned"))?;
            if turn.result.is_none() && turn.run_id.as_deref() == Some(&context.run_id) {
                controls.timeout = minimum(
                    controls.timeout,
                    turn.controls.timeout.map(|timeout| {
                        Duration::from_millis(timeout)
                            .saturating_sub(turn.started.elapsed())
                            .as_millis()
                            .min(u64::MAX as u128) as u64
                    }),
                );
            }
        }
        Ok(controls)
    }
    /// Host-only internal task using the ordinary child start and observation path.
    /// Reading a result retains its chat; acknowledge after external delivery or persistence.
    pub fn start_worker(
        &self,
        context: &ToolContext,
        profile: AgentProfile,
        input: Value,
    ) -> Result<Value> {
        ensure!(
            !profile.name.trim().is_empty(),
            "worker profile name must not be empty"
        );
        ensure!(
            profile.max_rounds != Some(0),
            "worker max_rounds must be positive"
        );
        self.cleanup_workers()?;
        self.start_profile_owned(context, input, None, Some(profile))
    }
    /// Internal worker launch preserving the originating run's remaining tool budget.
    pub fn start_worker_with_dispatcher(
        &self,
        context: &ToolContext,
        profile: AgentProfile,
        input: Value,
        dispatcher: ToolDispatcher,
    ) -> Result<Value> {
        ensure!(
            !profile.name.trim().is_empty(),
            "worker profile name must not be empty"
        );
        ensure!(
            profile.max_rounds != Some(0),
            "worker max_rounds must be positive"
        );
        self.cleanup_workers()?;
        self.start_profile_owned(context, input, Some(dispatcher), Some(profile))
    }
    /// The caller asserts that the already-read success was delivered or persisted.
    /// Failed, cancelled and timed-out workers remain available for retention.
    pub async fn acknowledge_worker(&self, owner: &str, address: &str) -> Result<()> {
        self.acknowledge_worker_owned(owner, address, None).await
    }
    /// A delivered result receipt may acknowledge only the run that produced it.
    pub async fn acknowledge_worker_turn(
        &self,
        owner: &str,
        address: &str,
        expected_run_id: &str,
    ) -> Result<()> {
        self.acknowledge_worker_owned(owner, address, Some(expected_run_id))
            .await
    }
    async fn acknowledge_worker_owned(
        &self,
        owner: &str,
        address: &str,
        expected_run_id: Option<&str>,
    ) -> Result<()> {
        let released = {
            let mut state = self
                .registry
                .state
                .lock()
                .map_err(|_| anyhow!("agent registry poisoned"))?;
            let retained = state
                .children
                .get(address)
                .or_else(|| {
                    state
                        .children
                        .values()
                        .find(|child| child.session_id == address)
                })
                .cloned();
            let session_id = retained
                .as_ref()
                .map_or(address, |child| child.session_id.as_str());
            let session = match self.store.session(session_id) {
                Ok(session) => session,
                Err(_) if retained.is_none() => return Ok(()), // idempotent acknowledgement by sessionId
                Err(error) => return Err(error),
            };
            ensure!(
                session.parent_session_id.as_deref() == Some(owner),
                "worker belongs to another conversation"
            );
            let (binding, _) = self
                .store
                .attachment(&session.id, BINDING_ID)?
                .ok_or_else(|| anyhow!("session is not an internal worker"))?;
            ensure!(
                serde_json::from_str::<Binding>(&binding)?.worker,
                "session is not an internal worker"
            );
            let latest = self.store.runs(&session.id)?.into_iter().last();
            if let Some(expected) = expected_run_id
                && let Some(run) = &latest
            {
                ensure!(
                    run.id == expected,
                    "worker run does not match acknowledgement receipt"
                );
            }
            let finished = worker_finished(
                &self.store,
                &session.id,
                latest.as_ref().map(|run| run.id.as_str()),
            )?
            .ok_or_else(|| anyhow!("worker has not finished cleanup"))?;
            ensure!(
                latest.as_ref().is_none_or(|run| run.id == finished.run_id),
                "worker completion does not match latest run"
            );
            ensure!(
                expected_run_id.is_none_or(|run| run == finished.run_id),
                "worker run does not match acknowledgement receipt"
            );
            if finished.status != "idle" {
                return Ok(());
            }
            if cancelled_run(&self.store, &session.id, Some(&finished.run_id))? {
                return Ok(());
            }
            ensure!(
                self.store
                    .attachment(&session.id, &format!("{READ_PREFIX}{}", finished.run_id))?
                    .is_some(),
                "worker result has not been read"
            );
            if self.limits().worker_keep_all {
                return Ok(());
            }
            let tree = subtree_ids(&self.store.list_sessions()?, &session.id);
            ensure!(
                tree.iter().all(|id| self
                    .store
                    .pending_effects(id)
                    .is_ok_and(|effects| effects.is_empty())),
                "worker subtree has unresolved effects and must be retained"
            );
            // A successful outer task must not erase failed or undelivered nested workers.
            for id in tree.iter().filter(|id| *id != &session.id) {
                let retained = state
                    .children
                    .values()
                    .find(|child| child.session_id == *id);
                if let Some(retained) = retained {
                    let turn = retained
                        .turn
                        .lock()
                        .map_err(|_| anyhow!("agent result poisoned"))?;
                    ensure!(
                        turn.result.is_some() && !turn.cancelling,
                        "worker subtree child cleanup is still in progress"
                    );
                }
                let latest = self.store.runs(id)?.into_iter().last();
                ensure!(
                    latest.as_ref().is_some_and(|run| run.status == "completed"),
                    "worker subtree contains unfinished or unsuccessful children"
                );
                ensure!(
                    !cancelled_run(&self.store, id, latest.as_ref().map(|run| run.id.as_str()))?,
                    "worker subtree contains cancelled children"
                );
                if let Some(retained) = retained {
                    let turn = retained
                        .turn
                        .lock()
                        .map_err(|_| anyhow!("agent result poisoned"))?;
                    ensure!(
                        turn.result.as_ref().is_some_and(|reply| matches!(
                            reply["status"].as_str(),
                            Some("idle" | "completed")
                        )),
                        "worker subtree contains unsuccessful children"
                    );
                }
                if let Some(run) = &latest {
                    ensure!(
                        self.store
                            .attachment(id, &format!("{FAILURE_PREFIX}{}", run.id))?
                            .is_none(),
                        "worker subtree contains child cleanup or control failures"
                    );
                }
                let binding = self
                    .store
                    .attachment(id, BINDING_ID)?
                    .map(|(metadata, _)| serde_json::from_str::<Binding>(&metadata))
                    .transpose()?;
                let finished =
                    worker_finished(&self.store, id, latest.as_ref().map(|run| run.id.as_str()))?;
                ensure!(
                    !binding.as_ref().is_some_and(|binding| binding.worker) || finished.is_some(),
                    "worker subtree contains workers awaiting cleanup publication"
                );
                if let Some(child) = finished {
                    ensure!(
                        latest.as_ref().is_some_and(|run| run.id == child.run_id),
                        "worker subtree contains stale completion metadata"
                    );
                    ensure!(
                        child.status == "idle"
                            && self
                                .store
                                .attachment(id, &format!("{READ_PREFIX}{}", child.run_id))?
                                .is_some(),
                        "worker subtree contains unsuccessful or unread workers"
                    );
                }
            }
            self.store.delete_session_preserving_effects(&session.id)?;
            self.registry
                .approval_sources
                .lock()
                .expect("child approval sources lock")
                .retain(|session, _| !tree.contains(session));
            let released: Vec<_> = state
                .children
                .values()
                .filter(|child| tree.contains(&child.session_id))
                .cloned()
                .collect();
            state
                .children
                .retain(|_, child| !tree.contains(&child.session_id));
            released
        };
        // Canonical deletion has committed; provider mirrors are released afterward.
        for child in released {
            // Hidden children already closed their mirror before publishing a result.
            if !child.hidden {
                child
                    .harness
                    .release_deleted_session(&child.session_id)
                    .await?;
            }
        }
        self.report_worktree_cleanup().await;
        Ok(())
    }
    pub(crate) async fn report_worktree_cleanup(&self) {
        report_worktree_cleanup(&self.store, &self.registry.output).await;
    }
    /// Delete expired terminal workers, preserving every unresolved effect boundary.
    pub fn cleanup_workers(&self) -> Result<usize> {
        cleanup_workers_at(
            &self.store,
            &self.registry,
            &self.limits(),
            chrono::Utc::now(),
        )
    }
    fn depth(&self, session: &str) -> Result<usize> {
        let mut current = self.store.session(session)?;
        let mut depth = 0usize;
        let mut seen = std::collections::HashSet::new();
        while let Some(parent) = current.parent_session_id {
            ensure!(seen.insert(parent.clone()), "session ancestry cycle");
            depth = depth
                .checked_add(1)
                .ok_or_else(|| anyhow!("session depth overflow"))?;
            current = self.store.session(&parent)?;
        }
        Ok(depth)
    }
    fn check_slots(&self, state: &State, owner: &str) -> Result<()> {
        ensure!(!state.stopping, "Agent host is shutting down");
        let active = state
            .children
            .values()
            .filter(|c| c.turn.lock().is_ok_and(|t| t.result.is_none()))
            .collect::<Vec<_>>();
        let limits = self.limits();
        if let Some(max) = limits.max_active_total {
            ensure!(
                active.len() < max,
                "host agent concurrency limit reached ({max})"
            );
        }
        if let Some(max) = limits.max_active_per_parent {
            ensure!(
                active.iter().filter(|c| c.owner == owner).count() < max,
                "parent agent concurrency limit reached ({max})"
            );
        }
        Ok(())
    }
    /// Retention bounds registry handles only. Evicting a finished child keeps
    /// its canonical session reachable by sessionId through the catalog.
    fn retain_slot(&self, state: &mut State) -> Result<()> {
        let Some(max) = self.limits().max_retained else {
            return Ok(());
        };
        ensure!(max > 0, "agent handle retention is disabled");
        while state.children.len() >= max {
            let oldest = state
                .children
                .iter()
                .filter(|(_, c)| c.turn.lock().is_ok_and(|t| t.result.is_some()))
                .min_by_key(|(_, c)| c.created)
                .map(|(h, _)| h.clone());
            if let Some(handle) = oldest {
                state.children.remove(&handle);
            } else {
                bail!("agent handle retention is full");
            }
        }
        Ok(())
    }
    fn start_owned(
        &self,
        context: &ToolContext,
        input: Value,
        dispatcher: Option<ToolDispatcher>,
    ) -> Result<Value> {
        self.start_profile_owned(context, input, dispatcher, None)
    }
    fn start_profile_owned(
        &self,
        context: &ToolContext,
        input: Value,
        dispatcher: Option<ToolDispatcher>,
        worker_profile: Option<AgentProfile>,
    ) -> Result<Value> {
        ensure!(!context.cancellation.is_cancelled(), "cancelled");
        let workspace_guard = crate::worktrees::admission_guard()?;
        let object = input
            .as_object()
            .ok_or_else(|| anyhow!("agent expects an object"))?;
        ensure!(
            object.keys().all(|k| matches!(
                k.as_str(),
                "task"
                    | "context"
                    | "profile"
                    | "output_schema"
                    | "max_tool_calls"
                    | "timeout"
                    | "worktree"
            )),
            "agent accepts only task, context, profile, output_schema, max_tool_calls, timeout and worktree"
        );
        let isolated = match input.get("worktree") {
            Some(Value::Bool(value)) => *value,
            None => false,
            _ => bail!("worktree must be a boolean"),
        };
        ensure!(
            !isolated || self.registry.workspace_host.get().is_some(),
            "worktree isolation is unavailable: this child host has no workspace binding"
        );
        let preparation_started = Instant::now();
        let inherited_timeout = self
            .inherited_controls(context, dispatcher.as_ref(), Controls::default())?
            .timeout;
        let mut controls =
            self.inherited_controls(context, dispatcher.as_ref(), Controls::parse(&input, None)?)?;
        let task = input["task"]
            .as_str()
            .ok_or_else(|| anyhow!("task must be a string"))?;
        ensure!(!task.trim().is_empty(), "task must not be empty");
        let supplied = input
            .get("context")
            .map(serde_json::to_string)
            .transpose()?
            .unwrap_or_default();
        ensure!(
            task.len().saturating_add(supplied.len()) <= MAX_CONTEXT_BYTES,
            "task and explicit context exceed 64 KiB"
        );
        let named = input
            .get("profile")
            .map(|v| {
                v.as_str()
                    .ok_or_else(|| anyhow!("profile must be a string"))
            })
            .transpose()?;
        let name = named.unwrap_or("read-only");
        if worker_profile.is_none()
            && let Some((metadata, _)) = self.store.attachment(&context.session_id, BINDING_ID)?
        {
            let parent_binding: Binding = serde_json::from_str(&metadata)?;
            ensure!(
                parent_binding.profile == "workspace"
                    || parent_binding.profile == name
                    || (parent_binding.worker && name == "read-only"),
                "nested child cannot broaden its parent's profile capabilities"
            );
        }
        let worker = worker_profile.is_some();
        ensure!(
            !worker || !isolated,
            "worktree isolation requires a registered child profile, rather than an internal worker profile"
        );
        let profile = match worker_profile.as_ref() {
            Some(profile) => profile,
            None => self
                .profile(name)
                .ok_or_else(|| anyhow!("agent profile {name} is not available"))?,
        };
        let parent_model = self
            .store
            .run(&context.run_id)
            .ok()
            .filter(|r| {
                r.session_id == context.session_id && r.model != bubblepaw_core::SCRIPT_MODEL
            })
            .map(|r| r.model);
        let binding = Binding {
            profile: profile.name.clone(),
            model: profile.model.clone().or(parent_model),
            max_rounds: profile.max_rounds,
            approval: profile.approval.clone(),
            worker,
        };
        let owner = self.store.session(&context.session_id)?;
        ensure!(!owner.read_only, "parent session is read only");
        if worker {
            profile
                .harness
                .store()
                .session(&owner.id)
                .map_err(|error| anyhow!("worker profile must use the owner's store: {error}"))?;
        }
        let depth = self.depth(&owner.id)?.saturating_add(1);
        if let Some(max) = self.limits().max_depth {
            ensure!(depth <= max, "agent depth limit reached ({max})");
        }
        ensure!(
            tokio::runtime::Handle::try_current().is_ok(),
            "Starting an agent requires a Tokio runtime"
        );
        let mut state = self
            .registry
            .state
            .lock()
            .map_err(|_| anyhow!("agent registry poisoned"))?;
        self.check_slots(&state, &owner.id)?;
        self.retain_slot(&mut state)?;
        let title_task = task.chars().take(80).collect::<String>();
        let session = self.store.create_session_with_visibility(
            format!(
                "Agent: {title_task} (parent {})",
                context.session_id.chars().take(8).collect::<String>()
            ),
            Default::default(),
            Some(&owner.id),
            worker || owner.hidden,
        )?;
        let workspace_runtime = (|| -> Result<_> {
            if isolated {
                let host = self
                    .registry
                    .workspace_host
                    .get()
                    .expect("validated workspace host");
                let parent = crate::worktrees::workspace(&self.store, &owner.id, &host.root)?;
                crate::worktrees::create_in_admission(&self.store, &parent, &session.id)?;
            }
            if worker {
                return Ok(None);
            }
            self.workspace_runtime(&session.id, &profile.name)
        })();
        let runtime = match workspace_runtime {
            Ok(runtime) => runtime,
            Err(error) => {
                // No run has started. Preserve a worktree whose setup succeeded
                // but whose adapters failed; report it instead of losing work.
                if crate::worktrees::get(&self.store, &session.id)?.is_none() {
                    self.store.delete_session(&session.id)?;
                }
                return Err(anyhow!(
                    "Could not prepare child {} workspace: {error:#}",
                    session.id
                ));
            }
        };
        let profile = runtime
            .as_ref()
            .map(|runtime| &runtime.profile)
            .unwrap_or(profile);
        if let Some(timeout) = inherited_timeout {
            let elapsed = preparation_started
                .elapsed()
                .as_millis()
                .min(u64::MAX as u128) as u64;
            controls.timeout = minimum(controls.timeout, Some(timeout.saturating_sub(elapsed)));
        }
        // Existing immutable attachment metadata stores the small host binding;
        // it never enters model history and requires no schema or core extension.
        if let Err(error) = self.store.store_attachment(
            &session.id,
            BINDING_ID,
            &serde_json::to_string(&binding)?,
            b"",
        ) {
            if worker
                && let Err(cleanup) = self.store.delete_session_preserving_effects(&session.id)
            {
                return Err(error.context(format!(
                    "Worker session {} retained because admission cleanup failed: {cleanup}",
                    session.id
                )));
            }
            return Err(error);
        }
        let prompt = if supplied.is_empty() {
            task.to_owned()
        } else {
            format!("Task:\n{task}\n\nExplicit context (JSON):\n{supplied}")
        };
        let events = profile.harness.subscribe();
        let started = Instant::now();
        let run = match self.admit_scoped(&session.id, || {
            profile.harness.start(
                &session.id,
                controlled_prompt(&prompt, &controls),
                self.child_run_options(
                    &session.id,
                    &profile.harness,
                    &binding,
                    clamp_approval(&context.approval, binding.approval.as_ref()),
                    &controls,
                    context,
                    dispatcher.as_ref(),
                ),
            )
        }) {
            Ok(run) => run,
            Err(error) => {
                if worker
                    && let Err(cleanup) = self.store.delete_session_preserving_effects(&session.id)
                {
                    return Err(error.context(format!(
                        "Worker session {} retained because admission cleanup failed: {cleanup}",
                        session.id
                    )));
                }
                return Err(error);
            }
        };
        let control_error = save_controls(&self.store, &session.id, &run.id, &controls)
            .err()
            .map(|e| e.to_string());
        let stop = CancellationToken::new();
        if control_error.is_some() {
            stop.cancel();
        }
        let handle = format!("agent:{}", uuid::Uuid::new_v4());
        let run_id = run.id.clone();
        let child = Arc::new(Child {
            owner: owner.id,
            session_id: session.id.clone(),
            handle: handle.clone(),
            created: Instant::now(),
            store: self.store.clone(),
            harness: profile.harness.clone(),
            runtime: runtime.clone(),
            binding: binding.clone(),
            original_profile: true,
            hidden: session.hidden,
            depth,
            turn: Mutex::new(Turn {
                result: None,
                unread: false,
                run_id: Some(run.id),
                stop,
                cancelling: false,
                controls,
                approval: clamp_approval(&context.approval, profile.approval.as_ref()),
                control_error,
                started,
                last_interaction: started,
                last_reminder: None,
                active_waits: 0,
            }),
            changed: Notify::new(),
            wake: Arc::downgrade(&self.registry.wake),
        });
        state.children.insert(handle.clone(), child.clone());
        drop(state);
        drop(workspace_guard);
        self.register_child_wake(&child);
        self.observe_turn(child, context, dispatcher, events);
        let mut receipt = json!({"handle":handle,"sessionId":session.id,"parentSessionId":context.session_id,"status":"running"});
        workspace_reply(&self.store, &session.id, &mut receipt)?;
        if worker {
            receipt["runId"] = json!(run_id);
        }
        Ok(receipt)
    }
    fn child(&self, owner: &str, address: &str) -> Result<Arc<Child>> {
        self.find_child(owner, address, true)
    }
    fn find_child(
        &self,
        owner: &str,
        address: &str,
        require_retention: bool,
    ) -> Result<Arc<Child>> {
        let mut state = self
            .registry
            .state
            .lock()
            .map_err(|_| anyhow!("agent registry poisoned"))?;
        if let Some(child) = state
            .children
            .get(address)
            .or_else(|| state.children.values().find(|c| c.session_id == address))
        {
            ensure!(
                child.owner == owner,
                "agent belongs to another conversation"
            );
            return Ok(child.clone());
        }
        ensure!(
            !address.starts_with("agent:"),
            "unknown or expired agent; use its sessionId to reattach"
        );
        let session = self.store.session(address)?;
        ensure!(
            session.parent_session_id.as_deref() == Some(owner),
            "agent belongs to another conversation"
        );
        let binding = match self.store.attachment(address, BINDING_ID)? {
            Some((metadata, _)) => serde_json::from_str::<Binding>(&metadata)?,
            // Old children have no binding; only the restricted default is safe.
            None => Binding {
                profile: "read-only".into(),
                model: None,
                max_rounds: self
                    .stored_profile("read-only")
                    .ok_or_else(|| anyhow!("legacy child requires read-only profile"))?
                    .max_rounds,
                approval: Some(ApprovalPolicy {
                    confirm: ApprovalMode::Deny,
                    tools: Default::default(),
                    deny: Default::default(),
                    timeout_secs: None,
                    hardline: true,
                }),
                worker: false,
            },
        };
        let original_profile = self.stored_profile(&binding.profile).is_some();
        let profile = self
            .stored_profile(&binding.profile)
            // A terminal worker may be inspected after restart without rebuilding
            // its dynamic profile. Workers never resume through the user send API.
            .or_else(|| binding.worker.then(|| self.profiles().first()).flatten())
            .ok_or_else(|| anyhow!("original agent profile {} is unavailable", binding.profile))?;
        let (runtime, workspace_unavailable) = if binding.worker {
            (None, false)
        } else {
            match self.workspace_runtime(address, &binding.profile) {
                Ok(runtime) => (runtime, false),
                Err(_) if !require_retention => (None, true),
                Err(error) => return Err(error),
            }
        };
        let profile = runtime
            .as_ref()
            .map(|runtime| &runtime.profile)
            .unwrap_or(profile);
        let mut latest = self.store.runs(address)?.into_iter().last();
        if let Some(run) = &mut latest {
            restore_text(&self.store, run)?;
        }
        ensure!(
            latest
                .as_ref()
                .is_none_or(|r| !matches!(r.status.as_str(), "running" | "waiting_for_quota")),
            "child has an unresolved run; recover the store before reattaching"
        );
        let retained = if workspace_unavailable {
            false
        } else {
            match self.retain_slot(&mut state) {
                Ok(()) => true,
                Err(error) if require_retention => return Err(error),
                Err(_) => false,
            }
        };
        let handle = format!("agent:{}", uuid::Uuid::new_v4());
        let mut result = latest
            .as_ref()
            .map(|r| terminal_reply(&handle, r))
            .unwrap_or_else(
                || json!({"handle":handle,"sessionId":address,"status":"idle","text":""}),
            );
        if latest
            .as_ref()
            .is_some_and(|r| matches!(r.status.as_str(), "interrupted" | "unknown"))
        {
            result["runStatus"] = result["status"].clone();
            result["status"] = json!("idle");
        }
        if let Some(run) = &latest
            && let Some((metadata, _)) = self
                .store
                .attachment(address, &format!("{FAILURE_PREFIX}{}", run.id))?
        {
            result = serde_json::from_str(&metadata)?;
            result["handle"] = json!(handle);
            result["sessionId"] = json!(address);
        }
        if result["status"] == "idle"
            && cancelled_run(&self.store, address, latest.as_ref().map(|r| r.id.as_str()))?
        {
            result["status"] = json!("cancelled");
        }
        let controls = latest
            .as_ref()
            .map(|run| load_controls(&self.store, address, &run.id))
            .transpose()?
            .unwrap_or_default();
        if result["status"] == "idle" {
            add_json(&mut result, &controls);
        }
        let run_id = latest.as_ref().map(|r| r.id.clone());
        let unread = match &run_id {
            Some(run_id) => self
                .store
                .attachment(address, &format!("{READ_PREFIX}{run_id}"))?
                .is_none(),
            None => false,
        };
        if !retained {
            result["handle"] = Value::Null;
        }
        let mut binding = binding;
        // Ordinary child round limits snapshot host policy, rather than a
        // caller-supplied child control. Refresh on reattachment so a removed
        // or raised profile limit does not survive in old bindings or wakes.
        // Internal worker profiles retain their task-specific frozen budget.
        if !binding.worker {
            binding.max_rounds = profile.max_rounds;
        }
        if binding.model.is_none() {
            binding.model = latest
                .map(|r| r.model)
                .filter(|m| m != bubblepaw_core::SCRIPT_MODEL);
        }
        let child = Arc::new(Child {
            owner: owner.into(),
            session_id: address.into(),
            handle: handle.clone(),
            created: Instant::now(),
            store: self.store.clone(),
            harness: profile.harness.clone(),
            runtime: runtime.clone(),
            binding: binding.clone(),
            original_profile,
            hidden: session.hidden,
            depth: self.depth(address)?,
            turn: Mutex::new(Turn {
                result: Some(result),
                unread,
                run_id,
                stop: CancellationToken::new(),
                cancelling: false,
                controls,
                approval: binding.approval.clone().unwrap_or_default(),
                control_error: None,
                started: Instant::now(),
                last_interaction: Instant::now(),
                last_reminder: None,
                active_waits: 0,
            }),
            changed: Notify::new(),
            wake: Arc::downgrade(&self.registry.wake),
        });
        if retained {
            state.children.insert(handle, child.clone());
        }
        Ok(child)
    }
    pub fn send(&self, context: &ToolContext, input: Value) -> Result<Value> {
        self.send_owned(context, input, None).map(|(v, _)| v)
    }
    /// Recover a quota wait as a fresh child run using its original profile,
    /// workspace and saved run controls. No user input is appended. The host
    /// supplies its current policy; every ancestor's restriction still applies.
    /// Admission errors leave the restart marker available to the scheduler.
    pub fn resume_quota_wait(
        &self,
        expected: &bubblepaw_core::QuotaWait,
        mut approval: ApprovalPolicy,
    ) -> Result<(Run, Arc<Harness>)> {
        let session_id = expected.session_id.as_str();
        let workspace_guard = crate::worktrees::admission_guard()?;
        if self.store.quota_wait(session_id)?.as_ref() != Some(expected) {
            return Err(bubblepaw_core::QuotaWaitChanged {
                session_id: session_id.to_owned(),
                run_id: expected.run_id.clone(),
            }
            .into());
        }
        let session = self.store.session(session_id)?;
        let owner = session
            .parent_session_id
            .as_deref()
            .ok_or_else(|| anyhow!("quota child recovery requires a child session"))?;
        ensure!(!session.read_only, "child session is read only");
        ensure!(
            !self.store.session(owner)?.read_only,
            "parent session is read only"
        );
        let (metadata, _) = self
            .store
            .attachment(session_id, BINDING_ID)?
            .ok_or_else(|| anyhow!("quota child recovery requires its original agent binding"))?;
        let binding: Binding = serde_json::from_str(&metadata)?;
        let latest = self
            .store
            .runs(session_id)?
            .into_iter()
            .last()
            .ok_or_else(|| anyhow!("quota child recovery requires an existing run"))?;
        ensure!(
            latest.id == expected.run_id,
            "quota wait is not the child's latest run"
        );
        ensure!(
            !cancelled_run(&self.store, session_id, Some(&latest.id))?,
            "child quota wait was cancelled"
        );
        let retained = self
            .registry
            .state
            .lock()
            .map_err(|_| anyhow!("agent registry poisoned"))?
            .children
            .values()
            .find(|child| child.session_id == session_id)
            .cloned();
        // Dynamic worker profiles are reusable only while their exact harness is
        // retained. find_child's terminal inspection fallback must never execute.
        if retained
            .as_ref()
            .is_none_or(|child| !child.original_profile)
        {
            ensure!(
                self.profile(&binding.profile).is_some(),
                "original agent profile {} is unavailable for quota recovery",
                binding.profile
            );
        }
        let mut current = Some(session_id.to_owned());
        let mut seen = HashSet::new();
        while let Some(id) = current {
            ensure!(seen.insert(id.clone()), "session ancestry contains a cycle");
            if let Some((metadata, _)) = self.store.attachment(&id, BINDING_ID)? {
                let ancestor: Binding = serde_json::from_str(&metadata)?;
                approval = clamp_approval(&approval, ancestor.approval.as_ref());
            }
            current = self.store.session(&id)?.parent_session_id;
        }
        if let Some(profile) = self.stored_profile(&binding.profile) {
            approval = clamp_approval(&approval, profile.approval.as_ref());
        }
        if self.store.data_dir().is_some() {
            crate::worktrees::restore_in_admission(&self.store, session_id)?;
        }
        let child = match retained {
            Some(child) => {
                ensure!(
                    child.owner == owner
                        && child.binding.profile == binding.profile
                        && child.binding.worker == binding.worker
                        && child.original_profile,
                    "retained child does not match its original agent binding"
                );
                child
            }
            None => self.child(owner, session_id)?,
        };
        let state = self
            .registry
            .state
            .lock()
            .map_err(|_| anyhow!("agent registry poisoned"))?;
        {
            let turn = child
                .turn
                .lock()
                .map_err(|_| anyhow!("agent result poisoned"))?;
            ensure!(
                !turn.cancelling,
                "child cancellation cleanup is still in progress"
            );
            if turn.result.is_none() {
                return Err(bubblepaw_core::SessionBusy {
                    session_id: session_id.to_owned(),
                    run_id: turn.run_id.clone().unwrap_or_default(),
                }
                .into());
            }
        }
        self.check_slots(&state, owner)?;
        let mut turn = child
            .turn
            .lock()
            .map_err(|_| anyhow!("agent result poisoned"))?;
        // Budgets and timeouts start afresh; the schema and configured limits
        // retain the same contract as an ordinary subsequent child turn.
        let controls = load_controls(&self.store, session_id, &latest.id)?;
        let context = ToolContext {
            session_id: owner.to_owned(),
            run_id: String::new(),
            invocation_id: String::new(),
            cancellation: CancellationToken::new(),
            approval: approval.clone(),
        };
        let mut resume_binding = child.binding.clone();
        if resume_binding.model.is_none() && latest.model != bubblepaw_core::SCRIPT_MODEL {
            resume_binding.model = Some(latest.model.clone());
        }
        let options = self.child_run_options(
            session_id,
            &child.harness,
            &resume_binding,
            approval.clone(),
            &controls,
            &context,
            None,
        );
        let events = child.harness.subscribe();
        let started = Instant::now();
        let run = self.admit_scoped(session_id, || {
            child.harness.start_quota_resume(expected, options)
        })?;
        let control_error = save_controls(&self.store, session_id, &run.id, &controls)
            .err()
            .map(|error| error.to_string());
        turn.result = None;
        turn.unread = false;
        turn.run_id = Some(run.id.clone());
        turn.stop = CancellationToken::new();
        if control_error.is_some() {
            turn.stop.cancel();
        }
        turn.controls = controls;
        turn.approval = approval;
        turn.control_error = control_error;
        turn.started = started;
        turn.last_interaction = started;
        turn.last_reminder = None;
        drop(turn);
        drop(state);
        drop(workspace_guard);
        self.registry.wake.user_input(session_id);
        self.register_child_wake(&child);
        self.observe_turn(child.clone(), &context, None, events);
        Ok((run, child.harness.clone()))
    }
    /// Host adapter follow-up preserving the originating run's remaining tool budget.
    pub fn send_with_dispatcher(
        &self,
        context: &ToolContext,
        input: Value,
        dispatcher: ToolDispatcher,
    ) -> Result<Value> {
        self.send_owned(context, input, Some(dispatcher))
            .map(|(value, _)| value)
    }
    fn send_owned(
        &self,
        context: &ToolContext,
        input: Value,
        dispatcher: Option<ToolDispatcher>,
    ) -> Result<(Value, bool)> {
        ensure!(!context.cancellation.is_cancelled(), "cancelled");
        let workspace_guard = crate::worktrees::admission_guard()?;
        let address = address(
            &input,
            &[
                "handle",
                "sessionId",
                "text",
                "question_id",
                "output_schema",
                "max_tool_calls",
                "timeout",
            ],
        )?;
        let text = input["text"]
            .as_str()
            .ok_or_else(|| anyhow!("text must be a string"))?;
        ensure!(!text.trim().is_empty(), "text must not be empty");
        ensure!(text.len() <= MAX_CONTEXT_BYTES, "text exceeds 64 KiB");
        ensure!(
            !self.store.session(&context.session_id)?.read_only,
            "parent session is read only"
        );
        let session_id = if address.starts_with("agent:") {
            self.registry
                .state
                .lock()
                .map_err(|_| anyhow!("agent registry poisoned"))?
                .children
                .get(address)
                .map(|child| child.session_id.clone())
                .ok_or_else(|| anyhow!("unknown or expired agent; use its sessionId to reattach"))?
        } else {
            address.to_owned()
        };
        ensure!(
            self.store
                .session(&session_id)?
                .parent_session_id
                .as_deref()
                == Some(&context.session_id),
            "agent belongs to another conversation"
        );
        if self.store.data_dir().is_some() {
            crate::worktrees::restore_in_admission(&self.store, &session_id)?;
        }
        let child = self.child(&context.session_id, address)?;
        let current_question = self.question_for_send(&child.session_id, &input)?;
        let mut recovered_answer = None;
        let mut resume_queued_answer = false;
        // Keep the observed question identity through atomic answer acceptance.
        // Re-reading only its pending status could turn a losing answer race
        // into unrelated child steering after the user resolves it.
        if let Some(question) = current_question
            .as_ref()
            .filter(|question| question["status"] == "pending")
        {
            ensure!(
                input.get("output_schema").is_none()
                    && input.get("max_tool_calls").is_none()
                    && input.get("timeout").is_none(),
                "run controls cannot change while answering a question"
            );
            let questions = self
                .registry
                .questions
                .get()
                .ok_or_else(|| anyhow!("question routing is unavailable"))?;
            let answer = questions.answer(
                &child.harness,
                &child.session_id,
                question["id"].as_str().unwrap_or_default(),
                text,
            )?;
            if answer.live {
                touch(&child)?;
                return Ok((
                    json!({"handle":child.handle,"sessionId":child.session_id,"status":"running","answered":true}),
                    false,
                ));
            }
            resume_queued_answer =
                answer_has_pending_input(&self.store, &child.session_id, &answer)?;
            if !resume_queued_answer {
                recovered_answer = Some(answer.continuation_reminder());
            }
        } else if let Some(questions) = self.registry.questions.get()
            && let Some(answer) = questions.unconsumed_answer(&child.harness, &child.session_id)?
        {
            let mut reminder = answer.continuation_reminder();
            reminder
                .text
                .push_str(&format!("\n\nParent message: {text}"));
            recovered_answer = Some(reminder);
        }
        ensure!(
            !child.binding.worker || recovered_answer.is_some() || resume_queued_answer,
            "internal workers are single-turn tasks"
        );
        let text = recovered_answer
            .as_ref()
            .map(|reminder| reminder.text.as_str())
            .unwrap_or(text);
        let parent_controls =
            self.inherited_controls(context, dispatcher.as_ref(), Controls::default())?;
        let state = self
            .registry
            .state
            .lock()
            .map_err(|_| anyhow!("agent registry poisoned"))?;
        ensure!(!state.stopping, "agent host is shutting down");
        let mut turn = child
            .turn
            .lock()
            .map_err(|_| anyhow!("agent result poisoned"))?;
        ensure!(
            !turn.cancelling,
            "child cancellation cleanup is still in progress"
        );
        if turn.result.is_none() {
            ensure!(
                input.get("output_schema").is_none()
                    && input.get("max_tool_calls").is_none()
                    && input.get("timeout").is_none(),
                "run controls can be changed only when starting a new child turn"
            );
            if let Some(reminder) = &recovered_answer {
                child
                    .harness
                    .steer_reminder(&child.session_id, reminder.clone())?;
            } else {
                child.harness.steer(&child.session_id, text)?;
            }
            turn.last_interaction = Instant::now();
            turn.last_reminder = None;
            return Ok((
                json!({"handle":child.handle,"sessionId":child.session_id,"status":"running","steered":true}),
                false,
            ));
        }
        let mut controls = Controls::parse(&input, turn.controls.output_schema.clone())?;
        controls.max_tool_calls = minimum(controls.max_tool_calls, parent_controls.max_tool_calls);
        controls.timeout = minimum(controls.timeout, parent_controls.timeout);
        let removed_budget =
            turn.controls.max_tool_calls.is_some() && controls.max_tool_calls.is_none();
        let prompt = if removed_budget {
            format!(
                "{text}\n\nThis turn has an unlimited tool-call budget; the previous turn's budget does not carry over."
            )
        } else {
            controlled_prompt(text, &controls)
        };
        // Own child is idle, so do not include it in the active-slot count.
        // Drop its result lock before the shared check reads all child turns.
        drop(turn);
        self.check_slots(&state, &context.session_id)?;
        let mut turn = child
            .turn
            .lock()
            .map_err(|_| anyhow!("agent result poisoned"))?;
        let profile = self
            .stored_profile(&child.binding.profile)
            .ok_or_else(|| anyhow!("agent profile unavailable"))?;
        let mut binding = child.binding.clone();
        binding.max_rounds = if binding.worker {
            minimum(binding.max_rounds, profile.max_rounds)
        } else {
            profile.max_rounds
        };
        let approval = clamp_approval(
            &clamp_approval(&context.approval, binding.approval.as_ref()),
            profile.approval.as_ref(),
        );
        let events = child.harness.subscribe();
        let started = Instant::now();
        let mut options = self.child_run_options(
            &child.session_id,
            &child.harness,
            &binding,
            approval.clone(),
            &controls,
            context,
            dispatcher.as_ref(),
        );
        if recovered_answer.is_some() || resume_queued_answer {
            options.origin = MessageOrigin::Generated;
        }
        let run = self.admit_scoped(&child.session_id, || {
            if resume_queued_answer {
                child.harness.start_resume(&child.session_id, options)
            } else if let Some(mut reminder) = recovered_answer {
                reminder.text = prompt;
                child
                    .harness
                    .start_reminders(&child.session_id, vec![reminder], options)
            } else {
                child.harness.start(&child.session_id, prompt, options)
            }
        })?;
        let control_error = save_controls(&self.store, &child.session_id, &run.id, &controls)
            .err()
            .map(|e| e.to_string());
        turn.result = None;
        turn.unread = false;
        turn.run_id = Some(run.id);
        turn.stop = CancellationToken::new();
        if control_error.is_some() {
            turn.stop.cancel();
        }
        turn.controls = controls;
        turn.approval = approval;
        turn.control_error = control_error;
        turn.started = started;
        turn.last_interaction = started;
        turn.last_reminder = None;
        drop(turn);
        drop(state);
        drop(workspace_guard);
        self.registry.wake.user_input(&child.session_id);
        self.register_child_wake(&child);
        self.observe_turn(child.clone(), context, dispatcher, events);
        Ok((
            json!({"handle":child.handle,"sessionId":child.session_id,"status":"running"}),
            true,
        ))
    }
    fn observe_turn(
        &self,
        child: Arc<Child>,
        context: &ToolContext,
        dispatcher: Option<ToolDispatcher>,
        mut events: broadcast::Receiver<HarnessEvent>,
    ) {
        let parent_cancel = context.cancellation.clone();
        let invocation = context.invocation_id.clone();
        let repl = child
            .runtime
            .as_ref()
            .map(|runtime| runtime.repl.clone())
            .or_else(|| self.repl.clone());
        let processes = child
            .runtime
            .as_ref()
            .map(|runtime| runtime.processes.clone())
            .or_else(|| self.processes.clone());
        let store = self.store.clone();
        let registry = self.registry.clone();
        let (stop, controls, started, control_error, run_id) = {
            let turn = child.turn.lock().expect("agent turn lock");
            (
                turn.stop.clone(),
                turn.controls.clone(),
                turn.started,
                turn.control_error.clone(),
                turn.run_id.clone().unwrap_or_default(),
            )
        };
        tokio::spawn(async move {
            let mut timed_out = false;
            let mut partial = String::new();
            let mut partial_bytes = 0u64;
            let mut partial_gap = false;
            let deadline = async {
                match controls.timeout {
                    Some(ms) => {
                        tokio::time::sleep(
                            Duration::from_millis(ms).saturating_sub(started.elapsed()),
                        )
                        .await
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::pin!(deadline);
            let run = loop {
                break tokio::select! {
                    biased;
                    run=child.harness.wait_for_idle(&child.session_id)=>run,
                    _=stop.cancelled()=> {
                        let descendants=registry.signal(&store,&child.session_id,false);
                        child.harness.cancel(&child.session_id);
                        AgentRegistry::finish_cancel(descendants).await;
                        child.harness.wait_for_idle(&child.session_id).await
                    },
                    _=parent_cancel.cancelled()=> {
                        stop.cancel();
                        let descendants=registry.signal(&store,&child.session_id,false);
                        child.harness.cancel(&child.session_id);
                        AgentRegistry::finish_cancel(descendants).await;
                        child.harness.wait_for_idle(&child.session_id).await
                    },
                    _=&mut deadline=> {
                        timed_out = true;
                        loop {
                            match events.try_recv() {
                                Ok(event) => capture_partial(event, &child.session_id, &run_id, &mut partial, &mut partial_bytes, &mut partial_gap),
                                Err(broadcast::error::TryRecvError::Lagged(_)) => partial_gap = true,
                                Err(_) => break,
                            }
                        }
                        if partial.is_empty() || partial_gap {
                            let snapshot = child.harness.snapshot_with(&child.session_id, || Ok(()))
                                .map(|(_, position)| position.streaming_text).unwrap_or_default();
                            if !snapshot.is_empty() {
                                partial_bytes = snapshot.len() as u64;
                                partial = snapshot;
                                partial_gap = false;
                            }
                        }
                        // The session and handle survive; only this run is cancelled.
                        child.harness.cancel(&child.session_id);
                        child.harness.wait_for_idle(&child.session_id).await
                    },
                    event=events.recv()=> {
                        match event {
                            Ok(event) => capture_partial(event, &child.session_id, &run_id, &mut partial, &mut partial_bytes, &mut partial_gap),
                            Err(broadcast::error::RecvError::Lagged(_)) => partial_gap = true,
                            Err(broadcast::error::RecvError::Closed) => {},
                        }
                        continue;
                    },
                };
            };
            // Even if run completion won the race, cancellation awaits descendants.
            if stop.is_cancelled() || timed_out {
                registry.cancel_children(&store, &child.session_id).await;
            }
            let canonical_terminal = run.as_ref().is_ok_and(|r| {
                store.run(&r.id).is_ok_and(|r| {
                    r.ended_at.is_some()
                        && !matches!(r.status.as_str(), "running" | "waiting_for_quota")
                })
            });
            if let Some(processes) = processes {
                processes.cancel_owner(&child.session_id).await;
            }
            let mut cleanup_error = match repl {
                Some(repl) => close_environments(&repl, &child.session_id).await.err(),
                None => None,
            };
            if child.hidden {
                let released = match tokio::time::timeout(
                    Duration::from_secs(1),
                    child.harness.release_idle_resources(&child.session_id),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(anyhow!("Worker provider cleanup timed out after 1 second")),
                };
                if let Err(error) = released {
                    cleanup_error = Some(match cleanup_error {
                        Some(previous) => anyhow!("{previous}; {error}"),
                        None => error,
                    });
                }
            }
            let observation_failed = run.is_err();
            let mut reply = match run {
                Ok(mut run) => {
                    let _ = restore_text(&store, &mut run);
                    terminal_reply(&child.handle, &run)
                }
                Err(e) => bounded_reply(
                    json!({"handle":child.handle,"sessionId":child.session_id,"status":"failed","text":"","error":e.to_string()}),
                ),
            };
            let canonical_text = reply["text"].clone();
            if stop.is_cancelled() && reply["status"] == "idle" {
                reply["status"] = json!("cancelled");
                if store
                    .attachment(&child.session_id, CANCEL_ID)
                    .is_ok_and(|m| m.is_none())
                    && let Err(error) =
                        store.store_attachment(&child.session_id, CANCEL_ID, "{}", b"")
                {
                    reply["error"] = json!(format!("Could not persist cancellation: {error}"));
                }
            }
            if timed_out {
                reply["status"] = json!("timed_out");
                if !partial.is_empty() {
                    reply["text"] = json!(partial);
                    if partial_bytes > reply["text"].as_str().unwrap_or("").len() as u64 {
                        reply["textBytes"] = json!(partial_bytes);
                        reply["resultTruncated"] = json!(true);
                    }
                    if partial_gap {
                        reply["textIncomplete"] = json!(true);
                    }
                }
                reply["error"] = json!(format!(
                    "Child run exceeded timeout of {} milliseconds",
                    controls.timeout.unwrap_or_default()
                ));
            } else if reply["status"] == "idle" {
                add_json(&mut reply, &controls);
            }
            if let Some(error) = control_error {
                reply["status"] = json!("failed");
                reply["error"] = json!(format!("Could not persist child run controls: {error}"));
            }
            if let Some(error) = &cleanup_error {
                reply["runStatus"] = reply["status"].clone();
                reply["status"] = json!("failed");
                let prior = reply["error"].as_str().unwrap_or("");
                reply["error"] = json!(format!(
                    "{prior}{}Child resource cleanup failed: {error}",
                    if prior.is_empty() { "" } else { "; " }
                ));
                reply = bounded_reply(reply);
            }
            reply = bounded_reply(reply);
            // An unfinished provider stream is a live result only. Durable
            // timeout metadata may retain completed canonical assistant text.
            let mut persisted_reply = reply.clone();
            if timed_out {
                persisted_reply["text"] = canonical_text;
                let object = persisted_reply.as_object_mut().expect("child reply object");
                for key in ["textBytes", "textIncomplete", "resultTruncated"] {
                    object.remove(key);
                }
                persisted_reply = bounded_reply(persisted_reply);
            }
            if (cleanup_error.is_some()
                || observation_failed
                || timed_out
                || reply["status"] == "failed")
                && let Ok(turn) = child.turn.lock()
                && let Some(run_id) = &turn.run_id
                && let Err(error) = store.store_attachment(
                    &child.session_id,
                    &format!("{FAILURE_PREFIX}{run_id}"),
                    &persisted_reply.to_string(),
                    b"",
                )
            {
                let prior = reply["error"].as_str().unwrap_or("");
                reply["error"] =
                    json!(format!("{prior}; Could not persist child failure: {error}"));
                reply = bounded_reply(reply);
            }
            if child.binding.worker {
                let finished = WorkerFinished {
                    run_id: run_id.clone(),
                    status: reply["status"].as_str().unwrap_or("failed").into(),
                    finished_at: chrono::Utc::now().to_rfc3339(),
                };
                if let Err(error) = store.store_attachment(
                    &child.session_id,
                    &format!("{WORKER_FINISHED_ID}:{run_id}"),
                    &serde_json::to_string(&finished).expect("worker completion metadata"),
                    b"",
                ) {
                    reply["status"] = json!("failed");
                    reply["error"] = json!(format!("Could not persist worker completion: {error}"));
                }
            }
            if canonical_terminal
                && cleanup_error.is_none()
                && let Some(dispatcher) = dispatcher
                && let Err(error) = dispatcher.complete_effect(&invocation)
            {
                tracing::warn!(
                    target: "bubblepaw::effects",
                    invocation = %invocation,
                    %error,
                    "could not complete a finished worker effect"
                );
            }
            let notice = if timed_out {
                // Completion notices can become durable generated parent input.
                // Keep unfinished streaming text exclusively in the live result.
                let mut canonical_reply = reply.clone();
                canonical_reply["text"] = persisted_reply["text"].clone();
                child_notice(&child, &run_id, &canonical_reply)
            } else {
                child_notice(&child, &run_id, &reply)
            };
            if let Ok(mut turn) = child.turn.lock() {
                turn.result = Some(reply);
                turn.unread = true;
            }
            if !child.binding.worker {
                registry.wake.publish_reminder_if(
                    &child.owner,
                    child_notice_key(&child.session_id, &run_id),
                    reminder_notices::child_completion(&child.session_id, &run_id, notice),
                    || {
                        child.turn.lock().is_ok_and(|turn| {
                            turn.unread && turn.run_id.as_deref() == Some(&run_id)
                        })
                    },
                );
            }
            child.changed.notify_waiters();
            let limits = registry.limits.get();
            let _ = cleanup_workers_at(&store, &registry, &limits, chrono::Utc::now());
            report_worktree_cleanup(&store, &registry.output).await;
        });
    }
    pub fn status(&self, owner: &str, input: &Value) -> Result<Value> {
        let object = input
            .as_object()
            .ok_or_else(|| anyhow!("agent_status expects an object"))?;
        ensure!(
            object
                .keys()
                .all(|k| matches!(k.as_str(), "handle" | "sessionId")),
            "agent_status accepts only handle or sessionId"
        );
        if object.is_empty() {
            let sessions = self.store.list_sessions()?;
            let ids = subtree_ids(&sessions, owner);
            let hidden_owner = self.store.session(owner)?.hidden;
            let children=sessions.into_iter().filter(|s| s.id != owner && ids.contains(&s.id) && (!s.hidden || hidden_owner)).map(|s| {
                let internal = self.store.attachment(&s.id, BINDING_ID)?.map(|(metadata, _)| serde_json::from_str::<Binding>(&metadata)).transpose()?.is_some_and(|binding| binding.worker);
                if internal { return Ok(None); }
                let retained={let state=self.registry.state.lock().map_err(|_| anyhow!("agent registry poisoned"))?;state.children.values().find(|c| c.session_id==s.id).cloned()};
                let reply=match retained {Some(child)=>{touch(&child)?;snapshot(&child,false)?},None=>self.archived_status(&s.id)?};
                Ok(Some(json!({"handle":reply["handle"],"sessionId":reply["sessionId"],"parentSessionId":s.parent_session_id,"status":reply["status"],"state":reply["state"],"unread":reply["unread"],"workspace":reply["workspace"],"branch":reply["branch"],"worktree":reply["worktree"]})))
            }).collect::<Result<Vec<_>>>()?.into_iter().flatten().collect::<Vec<_>>();
            return Ok(json!({"children":children}));
        }
        let child = self.find_child(owner, address(input, &["handle", "sessionId"])?, false)?;
        snapshot(&child, true)
    }
    fn archived_status(&self, session: &str) -> Result<Value> {
        let latest = self.store.runs(session)?.into_iter().last();
        let mut state = latest
            .as_ref()
            .map(|run| match run.status.as_str() {
                "completed" | "interrupted" => "idle",
                "running" => "running",
                "waiting_for_quota" => "waiting_for_quota",
                "cancelled" => "cancelled",
                _ => "failed",
            })
            .unwrap_or("idle")
            .to_owned();
        let mut unread = false;
        if let Some(run) = latest {
            unread = self
                .store
                .attachment(session, &format!("{READ_PREFIX}{}", run.id))?
                .is_none();
            if let Some((metadata, _)) = self
                .store
                .attachment(session, &format!("{FAILURE_PREFIX}{}", run.id))?
            {
                let reply: Value = serde_json::from_str(&metadata)?;
                state = reply["status"].as_str().unwrap_or("failed").into();
            }
        }
        let latest_id = self.store.runs(session)?.into_iter().last().map(|r| r.id);
        if state == "idle" && cancelled_run(&self.store, session, latest_id.as_deref())? {
            state = "cancelled".into();
        }
        let mut reply = json!({"handle":null,"sessionId":session,"parentSessionId":self.store.session(session)?.parent_session_id,"status":state,"state":state,"unread":unread});
        workspace_reply(&self.store, session, &mut reply)?;
        if let Some(wait) = self.store.quota_wait(session)? {
            reply["status"] = json!("waiting_for_quota");
            reply["state"] = json!("waiting_for_quota");
            reply["quotaWait"] = serde_json::to_value(wait)?;
        }
        if let Some(question) = pending_question(&self.store, session)? {
            reply["status"] = json!("waiting_for_answer");
            reply["state"] = json!("waiting_for_answer");
            reply["question"] = question;
        }
        Ok(reply)
    }
    pub async fn wait(&self, owner: &str, address: &str, timeout_ms: Option<u64>) -> Result<Value> {
        let child = self.find_child(owner, address, false)?;
        let _interaction = WaitInteraction::new(child.clone())?;
        if let Some(ms) = timeout_ms {
            if tokio::time::timeout(
                Duration::from_millis(ms),
                await_child(&child, self.registry.questions.get().map(Arc::as_ref)),
            )
            .await
            .is_err()
            {
                return Ok(
                    json!({"handle":child.handle,"sessionId":child.session_id,"status":"timeout"}),
                );
            }
        } else {
            await_child(&child, self.registry.questions.get().map(Arc::as_ref)).await?;
        }
        let mut reply = snapshot(&child, true)?;
        if reply["status"] == "idle" {
            reply["state"] = json!("idle");
            reply["status"] = json!("completed");
        }
        reply
            .as_object_mut()
            .expect("agent reply object")
            .remove("unread");
        Ok(reply)
    }
    pub async fn cancel(&self, owner: &str, address: &str) -> Result<Value> {
        let child = self.find_child(owner, address, false)?;
        let cancelled_run_id = child
            .turn
            .lock()
            .map_err(|_| anyhow!("agent result poisoned"))?
            .run_id
            .clone();
        self.registry.wake.suppress(&child.session_id);
        self.mark_catalog_cancel(&child.session_id, true)?;
        self.registry
            .cancel_tree(&self.store, &child.session_id, true)
            .await;
        // A temporary result reader may not be retained when slots are busy.
        // Apply the intent to that snapshot, without touching a newer run.
        if let Ok(mut turn) = child.turn.lock()
            && turn.run_id == cancelled_run_id
            && let Some(reply) = turn.result.as_mut()
            && reply["status"] == "idle"
        {
            reply["status"] = json!("cancelled");
        }
        let mut reply = snapshot(&child, false)?;
        reply
            .as_object_mut()
            .expect("agent reply object")
            .remove("unread");
        Ok(reply)
    }
    /// Synchronous harness hook signals the subtree deepest first. The running
    /// turn observers wait for descendants before publishing terminal results.
    pub fn cancel_session(&self, owner: &str) -> usize {
        // A tree signal calls Harness::cancel on each child. Its nested tool
        // hook must not traverse that already-signalled tree again before the
        // harness gets to cancel its own run. The durable intent was written
        // for the complete subtree before its first signal.
        let signalled = self.registry.state.lock().is_ok_and(|state| {
            state.children.values().any(|c| {
                c.session_id == owner && c.turn.lock().is_ok_and(|t| t.stop.is_cancelled())
            })
        });
        if signalled
            && self
                .store
                .attachment(owner, CANCEL_ID)
                .is_ok_and(|m| m.is_some())
        {
            return 0;
        }
        let _ = self.mark_catalog_cancel(owner, false);
        let children = self.registry.signal(&self.store, owner, false);
        let count = children.len();
        if !children.is_empty()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(AgentRegistry::finish_cancel(children));
        }
        count
    }
    /// Persist cancellation intent for every canonical descendant, even when
    /// its inactive handle expired. Resources are still awaited separately.
    fn mark_catalog_cancel(&self, owner: &str, include_owner: bool) -> Result<()> {
        let ids = subtree_ids(&self.store.list_sessions()?, owner);
        for id in ids.into_iter().filter(|id| include_owner || id != owner) {
            if let Some(questions) = self.registry.questions.get() {
                questions.cancel(&id)?;
            }
            if self.store.attachment(&id, CANCEL_ID)?.is_none() {
                self.store.store_attachment(&id, CANCEL_ID, "{}", b"")?;
            }
            if let Some(run) = self.store.runs(&id)?.into_iter().last() {
                let marker = format!("{CANCEL_PREFIX}{}", run.id);
                if self.store.attachment(&id, &marker)?.is_none() {
                    self.store.store_attachment(&id, &marker, "{}", b"")?;
                }
            }
        }
        Ok(())
    }
    pub async fn cancel_children(&self, owner: &str) -> Result<()> {
        self.mark_catalog_cancel(owner, false)?;
        self.registry.cancel_children(&self.store, owner).await;
        Ok(())
    }
    pub async fn shutdown(&self) -> Result<()> {
        let children = {
            let mut state = self
                .registry
                .state
                .lock()
                .map_err(|_| anyhow!("agent registry poisoned"))?;
            state.stopping = true;
            let mut children = state
                .children
                .values()
                .filter(|c| c.turn.lock().is_ok_and(|t| t.result.is_none()))
                .cloned()
                .collect::<Vec<_>>();
            children.sort_by_key(|c| std::cmp::Reverse(c.depth));
            children
                .into_iter()
                .map(|child| {
                    let run_id = child.turn.lock().ok().and_then(|mut turn| {
                        turn.cancelling = true;
                        turn.stop.cancel();
                        turn.run_id.clone()
                    });
                    CancelledTurn { child, run_id }
                })
                .collect::<Vec<_>>()
        };
        AgentRegistry::finish_cancel(children).await;
        Ok(())
    }
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
fn run_options(binding: &Binding, approval: ApprovalPolicy, controls: &Controls) -> RunOptions {
    RunOptions {
        origin: MessageOrigin::ChildTask,
        model: binding.model.clone(),
        max_rounds: binding.max_rounds,
        approval: Some(approval),
        output_schema: controls.output_schema.clone(),
        max_tool_calls: controls.max_tool_calls,
        ..Default::default()
    }
}
/// Deny < Ask < Auto < Allow, applied to default Confirm and every named rule.
pub fn clamp_approval(parent: &ApprovalPolicy, profile: Option<&ApprovalPolicy>) -> ApprovalPolicy {
    let Some(profile) = profile else {
        return parent.clone();
    };
    let mut policy = ApprovalPolicy {
        confirm: stricter(parent.confirm, profile.confirm),
        tools: Default::default(),
        deny: profile.clone().with_inherited_denies(parent).deny,
        timeout_secs: parent.timeout_secs,
        // Children inherit the host's hardline floor; a profile cannot turn it off.
        hardline: parent.hardline,
    };
    for name in parent.tools.keys().chain(profile.tools.keys()) {
        // A named override may address a confirming capability. Clamp against
        // that default too, conservatively tightening non-confirming tools.
        policy.tools.insert(
            name.clone(),
            stricter(
                parent.mode(name, Approval::Confirm),
                profile.mode(name, Approval::Confirm),
            ),
        );
    }
    policy
}
fn stricter(a: ApprovalMode, b: ApprovalMode) -> ApprovalMode {
    fn rank(m: ApprovalMode) -> u8 {
        match m {
            ApprovalMode::Deny => 0,
            ApprovalMode::Ask => 1,
            ApprovalMode::Auto => 2,
            ApprovalMode::Allow => 3,
        }
    }
    if rank(a) <= rank(b) { a } else { b }
}
impl AgentTools {
    fn profile_summaries(&self) -> Vec<(String, String)> {
        self.profiles()
            .iter()
            .filter(|profile| self.profile_enabled(&profile.name))
            .map(|p| (p.name.clone(), p.description.clone()))
            .collect()
    }
}
fn cap(limit: Option<usize>) -> String {
    limit.map_or_else(|| "unlimited".into(), |value| value.to_string())
}
fn minimum<T: Ord>(requested: Option<T>, inherited: Option<T>) -> Option<T> {
    match (requested, inherited) {
        (Some(requested), Some(inherited)) => Some(requested.min(inherited)),
        (requested, inherited) => requested.or(inherited),
    }
}
fn worker_finished(
    store: &Store,
    session: &str,
    run: Option<&str>,
) -> Result<Option<WorkerFinished>> {
    let current = run
        .map(|run| store.attachment(session, &format!("{WORKER_FINISHED_ID}:{run}")))
        .transpose()?
        .flatten();
    current
        .or(store.attachment(session, WORKER_FINISHED_ID)?)
        .map(|(metadata, _)| serde_json::from_str(&metadata).map_err(Into::into))
        .transpose()
}
fn validate_profiles(profiles: &[AgentProfile]) -> Result<()> {
    ensure!(
        !profiles.is_empty(),
        "at least one agent profile is required"
    );
    let mut names = HashSet::new();
    for profile in profiles {
        ensure!(
            !profile.name.trim().is_empty(),
            "agent profile name must not be empty"
        );
        ensure!(
            names.insert(profile.name.clone()),
            "duplicate agent profile: {}",
            profile.name
        );
        ensure!(
            profile.max_rounds != Some(0),
            "agent profile max_rounds must be positive"
        );
    }
    Ok(())
}
/// Session ids in `owner`'s catalog subtree, including `owner`.
fn subtree_ids(sessions: &[Session], owner: &str) -> HashSet<String> {
    let mut ids = HashSet::from([owner.to_owned()]);
    loop {
        let before = ids.len();
        for session in sessions {
            if session
                .parent_session_id
                .as_ref()
                .is_some_and(|p| ids.contains(p))
            {
                ids.insert(session.id.clone());
            }
        }
        if before == ids.len() {
            return ids;
        }
    }
}
fn address<'a>(input: &'a Value, allowed: &[&str]) -> Result<&'a str> {
    let object = input
        .as_object()
        .ok_or_else(|| anyhow!("agent operation expects an object"))?;
    ensure!(
        object.keys().all(|k| allowed.contains(&k.as_str())),
        "unexpected agent argument"
    );
    ensure!(
        input.get("handle").is_none() || input.get("sessionId").is_none(),
        "provide either handle or sessionId"
    );
    let value = input
        .get("handle")
        .or_else(|| input.get("sessionId"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("handle or sessionId must be a string"))?;
    ensure!(!value.is_empty(), "handle or sessionId must not be empty");
    Ok(value)
}
fn snapshot(child: &Child, consume: bool) -> Result<Value> {
    if consume && let Some(wake) = child.wake.upgrade() {
        return wake.consume(&child.owner, || snapshot_inner(child, true));
    }
    snapshot_inner(child, consume).map(|(reply, _)| reply)
}
fn snapshot_inner(child: &Child, consume: bool) -> Result<(Value, Option<String>)> {
    let mut turn = child
        .turn
        .lock()
        .map_err(|_| anyhow!("agent result poisoned"))?;
    if consume {
        turn.last_interaction = Instant::now();
        turn.last_reminder = None;
    }
    let mut reply = turn.result.clone().unwrap_or_else(
        || json!({"handle":child.handle,"sessionId":child.session_id,"status":"running"}),
    );
    let quota = child.store.quota_wait(&child.session_id)?;
    if let Some(wait) = &quota {
        reply["status"] = json!("waiting_for_quota");
        reply["quotaWait"] = serde_json::to_value(wait)?;
    }
    if let Some(question) = pending_question(&child.store, &child.session_id)? {
        reply["status"] = json!("waiting_for_answer");
        reply["question"] = question;
    }
    reply["state"] = reply["status"].clone();
    reply["parentSessionId"] = json!(child.owner);
    reply["unread"] = json!(turn.unread);
    if consume && turn.result.is_some() && quota.is_none() {
        if turn.unread
            && let Some(run_id) = &turn.run_id
        {
            let receipt = format!("{READ_PREFIX}{run_id}");
            if child
                .store
                .attachment(&child.session_id, &receipt)?
                .is_none()
            {
                child
                    .store
                    .store_attachment(&child.session_id, &receipt, "{}", b"")?;
            }
        }
        turn.unread = false;
    }
    let source = if consume && turn.result.is_some() && quota.is_none() {
        turn.run_id
            .as_ref()
            .map(|run| child_notice_key(&child.session_id, run))
    } else {
        None
    };
    workspace_reply(&child.store, &child.session_id, &mut reply)?;
    Ok((bounded_reply(reply), source))
}
fn workspace_reply(store: &Store, session: &str, reply: &mut Value) -> Result<()> {
    let mut current = Some(session.to_owned());
    let mut seen = HashSet::new();
    while let Some(id) = current {
        ensure!(seen.insert(id.clone()), "session ancestry contains a cycle");
        if let Some(info) = crate::worktrees::get(store, &id)? {
            reply["workspace"] = json!(info.path);
            reply["branch"] = json!(info.branch);
            reply["worktreeAvailable"] = json!(info.path.is_dir());
            reply["worktree"] = serde_json::to_value(info)?;
            reply["note"] = if id == session {
                json!("Parent uncommitted changes are not copied into the child worktree.")
            } else {
                json!("Uses the parent's existing worktree; uncommitted changes are shared.")
            };
            break;
        }
        current = store.session(&id)?.parent_session_id;
    }
    Ok(())
}
fn cleanup_workers_at(
    store: &Store,
    registry: &AgentRegistry,
    limits: &Agents,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<usize> {
    if limits.worker_keep_all {
        return Ok(0);
    }
    let cutoff = now
        .checked_sub_signed(chrono::Duration::days(i64::from(
            limits.worker_retention_days,
        )))
        .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC);
    let sessions = store.list_sessions()?;
    let mut state = registry
        .state
        .lock()
        .map_err(|_| anyhow!("agent registry poisoned"))?;
    let mut deleted = HashSet::new();
    for session in &sessions {
        if registry
            .scope
            .as_ref()
            .is_some_and(|scope| !scope.contains(store, &session.id))
            || !session.hidden
            || deleted.contains(&session.id)
            || store.active_session_metadata(&session.id)?.is_none()
        {
            continue;
        }
        let Some((metadata, _)) = store.attachment(&session.id, BINDING_ID)? else {
            continue;
        };
        if !serde_json::from_str::<Binding>(&metadata)?.worker {
            continue;
        }
        let mut tree = subtree_ids(&sessions, &session.id);
        // Earlier candidates can delete a descendant before its ancestor is
        // inspected. Preserve ancestry from the snapshot, but exclude removed
        // records from checks and deletion counts.
        tree.retain(|id| !deleted.contains(id));
        let mut eligible = true;
        for id in &tree {
            let retained = state
                .children
                .values()
                .find(|child| child.session_id == *id);
            if let Some(child) = retained {
                let turn = child
                    .turn
                    .lock()
                    .map_err(|_| anyhow!("agent result poisoned"))?;
                if turn.result.is_none() || (limits.worker_retention_days == 0 && turn.unread) {
                    eligible = false;
                    break;
                }
            }
            if store.quota_wait(id)?.is_some() || !store.pending_effects(id)?.is_empty() {
                eligible = false;
                break;
            }
            let latest = store.runs(id)?.into_iter().last();
            if latest
                .as_ref()
                .is_some_and(|run| matches!(run.status.as_str(), "running" | "waiting_for_quota"))
            {
                eligible = false;
                break;
            }
            let finished = worker_finished(store, id, latest.as_ref().map(|run| run.id.as_str()))?;
            if finished
                .as_ref()
                .zip(latest.as_ref())
                .is_some_and(|(finished, run)| finished.run_id != run.id)
            {
                eligible = false;
                break;
            }
            let cancelled = cancelled_run(
                store,
                id,
                finished
                    .as_ref()
                    .map(|value| value.run_id.as_str())
                    .or_else(|| latest.as_ref().map(|run| run.id.as_str())),
            )?;
            let internal_worker = if *id == session.id {
                true
            } else {
                store
                    .attachment(id, BINDING_ID)?
                    .map(|(metadata, _)| serde_json::from_str::<Binding>(&metadata))
                    .transpose()?
                    .is_some_and(|binding| binding.worker)
            };
            // Only explicit acknowledgement may delete successful workers.
            // A crashed observer without a completion marker is conservative too.
            if internal_worker
                && !cancelled
                && (finished
                    .as_ref()
                    .is_some_and(|value| value.status == "idle")
                    || (finished.is_none()
                        && latest.as_ref().is_some_and(|run| run.status == "completed")))
            {
                eligible = false;
                break;
            }
            let ended = finished
                .as_ref()
                .map(|value| value.finished_at.as_str())
                .or_else(|| latest.as_ref().and_then(|run| run.ended_at.as_deref()))
                .unwrap_or_else(|| {
                    sessions
                        .iter()
                        .find(|session| session.id == *id)
                        .map_or(session.created_at.as_str(), |session| {
                            session.created_at.as_str()
                        })
                });
            if chrono::DateTime::parse_from_rfc3339(ended)?.with_timezone(&chrono::Utc) > cutoff {
                eligible = false;
                break;
            }
        }
        if eligible {
            // Admission and unresolved-effect checks repeat atomically at deletion.
            store.delete_session_preserving_effects(&session.id)?;
            state
                .children
                .retain(|_, child| !tree.contains(&child.session_id));
            deleted.extend(tree);
        }
    }
    if !deleted.is_empty() {
        registry
            .approval_sources
            .lock()
            .expect("child approval sources lock")
            .retain(|session, _| !deleted.contains(session));
    }
    Ok(deleted.len())
}
fn touch(child: &Child) -> Result<()> {
    let mut turn = child
        .turn
        .lock()
        .map_err(|_| anyhow!("agent result poisoned"))?;
    turn.last_interaction = Instant::now();
    turn.last_reminder = None;
    Ok(())
}
struct WaitInteraction(Arc<Child>);
impl WaitInteraction {
    fn new(child: Arc<Child>) -> Result<Self> {
        {
            let mut turn = child
                .turn
                .lock()
                .map_err(|_| anyhow!("agent result poisoned"))?;
            turn.active_waits += 1;
            turn.last_interaction = Instant::now();
            turn.last_reminder = None;
        }
        Ok(Self(child))
    }
}
impl Drop for WaitInteraction {
    fn drop(&mut self) {
        if let Ok(mut turn) = self.0.turn.lock() {
            turn.active_waits = turn.active_waits.saturating_sub(1);
            turn.last_interaction = Instant::now();
            turn.last_reminder = None;
        }
    }
}
fn load_controls(store: &Store, session: &str, run: &str) -> Result<Controls> {
    store
        .attachment(session, &format!("{CONTROLS_PREFIX}{run}"))?
        .map(|(metadata, _)| serde_json::from_str(&metadata))
        .transpose()
        .map(|v| v.unwrap_or_default())
        .map_err(Into::into)
}
fn save_controls(store: &Store, session: &str, run: &str, controls: &Controls) -> Result<()> {
    store
        .store_attachment(
            session,
            &format!("{CONTROLS_PREFIX}{run}"),
            &serde_json::to_string(controls)?,
            b"",
        )
        .map(|_| ())
}
fn controlled_prompt(text: &str, controls: &Controls) -> String {
    match controls.max_tool_calls {
        Some(max) => format!(
            "{text}\n\nYou have {max} tool calls for this turn. Each outer JS tool call and each nested host tool call counts individually. Plan within the budget and return your final answer when it is exhausted."
        ),
        None => text.to_owned(),
    }
}
fn capture_partial(
    event: HarnessEvent,
    session: &str,
    run: &str,
    partial: &mut String,
    bytes: &mut u64,
    gap: &mut bool,
) {
    if event.session_id != session || event.run_id != run {
        return;
    }
    match event.kind {
        EventKind::ModelRequestStarted {} => {
            partial.clear();
            *bytes = 0;
            *gap = false;
        }
        EventKind::TextDelta { text } => {
            *bytes = bytes.saturating_add(text.len() as u64);
            let mut end = text
                .len()
                .min(MAX_REPLY_BYTES.saturating_sub(partial.len()));
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            partial.push_str(&text[..end]);
        }
        // The streamed assistant tool round is canonical now; a later provider
        // response starts a fresh partial answer.
        EventKind::ToolStarted { .. } => {
            partial.clear();
            *bytes = 0;
        }
        _ => {}
    }
}
fn cancelled_run(store: &Store, session: &str, run: Option<&str>) -> Result<bool> {
    if let Some(run) = run {
        if store
            .attachment(session, &format!("{CANCEL_PREFIX}{run}"))?
            .is_some()
        {
            return Ok(true);
        }
        // A new run explicitly supersedes legacy whole-session cancellation.
        if store
            .attachment(session, &format!("{CONTROLS_PREFIX}{run}"))?
            .is_some()
        {
            return Ok(false);
        }
    }
    Ok(store.attachment(session, CANCEL_ID)?.is_some())
}
fn add_json(reply: &mut Value, controls: &Controls) {
    if controls.output_schema.is_none() {
        return;
    }
    match serde_json::from_str::<Value>(reply["text"].as_str().unwrap_or("")) {
        Ok(value) => {
            reply["json"] = value;
            // Reserve the small status/read-receipt fields added by snapshots.
            if reply.to_string().len().saturating_add(128) > MAX_REPLY_BYTES {
                reply
                    .as_object_mut()
                    .expect("child reply object")
                    .remove("json");
                reply["status"] = json!("failed");
                reply["error"] = json!(
                    "Validated JSON exceeds child reply size limit of 60 KiB; full text remains in child history"
                );
            }
        }
        Err(error) => {
            reply["status"] = json!("failed");
            reply["error"] = json!(format!(
                "Validated child output could not be parsed as JSON: {error}"
            ));
        }
    }
}
async fn await_child(child: &Child, questions: Option<&QuestionTools>) -> Result<()> {
    loop {
        let changed = child.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        let mut question_changed =
            questions.map(|questions| Box::pin(questions.changed.notified()));
        if let Some(changed) = &mut question_changed {
            changed.as_mut().enable();
        }
        if pending_question(&child.store, &child.session_id)?.is_some() {
            return Ok(());
        }
        if child
            .turn
            .lock()
            .map_err(|_| anyhow!("agent result poisoned"))?
            .result
            .is_some()
        {
            return Ok(());
        }
        tokio::select! {
            _ = changed => {},
            _ = async { if let Some(changed) = question_changed { changed.await } else { std::future::pending::<()>().await } } => {},
        }
    }
}
async fn await_cancelled_turn(child: &Child, run_id: &Option<String>) -> Result<()> {
    loop {
        let changed = child.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        {
            let turn = child
                .turn
                .lock()
                .map_err(|_| anyhow!("agent result poisoned"))?;
            if &turn.run_id != run_id || turn.result.is_some() {
                return Ok(());
            }
        }
        changed.await;
    }
}
/// Store run records deliberately omit text; restore the last run's result
/// from canonical history using the same time boundaries as core hydration.
fn restore_text(store: &Store, run: &mut Run) -> Result<()> {
    if run.text.is_some() {
        return Ok(());
    }
    let start = chrono::DateTime::parse_from_rfc3339(&run.started_at)?;
    let end = chrono::DateTime::parse_from_rfc3339(
        run.ended_at
            .as_deref()
            .ok_or_else(|| anyhow!("Completed run has no end time"))?,
    )?;
    for message in store.messages(&run.session_id)?.into_iter().rev() {
        let created = chrono::DateTime::parse_from_rfc3339(&message.created_at)?;
        if created < start {
            break;
        }
        if created <= end
            && (message.role == Role::Assistant
                || (run.model == bubblepaw_core::SCRIPT_MODEL && message.role == Role::Tool))
        {
            run.text = Some(message.content);
            break;
        }
    }
    Ok(())
}
fn terminal_reply(handle: &str, run: &Run) -> Value {
    let status = match run.status.as_str() {
        "completed" => "idle",
        "cancelled" => "cancelled",
        _ => "failed",
    };
    let mut value = json!({"handle":handle,"sessionId":run.session_id,"status":status,"text":run.text.as_deref().unwrap_or("")});
    if let Some(error) = &run.error {
        value["error"] = json!(error);
    }
    if let Some(usage) = &run.usage {
        value["usage"] = json!(usage);
    }
    value
}
async fn close_environments(repl: &CodeMode, session: &str) -> Result<()> {
    let started = Instant::now();
    let mut last_error = String::new();
    loop {
        // Re-read inventory so a concurrent child continuation cannot introduce
        // an unclosed environment after a one-time snapshot.
        let inventory = repl.status(session);
        let environments = inventory["environments"]
            .as_array()
            .ok_or_else(|| anyhow!("Invalid child environment inventory"))?;
        if environments.is_empty() {
            return Ok(());
        }
        ensure!(
            started.elapsed() < Duration::from_secs(5),
            "Timed out closing child environments after 5 seconds{}",
            if last_error.is_empty() {
                String::new()
            } else {
                format!(": {last_error}")
            }
        );
        for environment in environments
            .iter()
            .filter(|environment| environment["status"] != "closing")
        {
            let name = environment["name"]
                .as_str()
                .ok_or_else(|| anyhow!("Child environment has no name"))?;
            if let Err(error) = repl.control(
                session,
                json!({"action":"close","environment":name,"cancel":true}),
            ) {
                last_error = error.to_string().chars().take(512).collect();
            }
        }
        // status reaps closed threads. Cancellation wakes explicit waits; the
        // deadline above reports failure instead of waiting indefinitely.
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn bounded_reply(mut value: Value) -> Value {
    if value.get("json").is_some() && value.to_string().len() > MAX_REPLY_BYTES {
        value
            .as_object_mut()
            .expect("child reply object")
            .remove("json");
        value["status"] = json!("failed");
        value["error"] = json!(
            "Validated JSON exceeds child reply size limit of 60 KiB; full text remains in child history"
        );
    }
    let original_text = value["textBytes"]
        .as_u64()
        .unwrap_or_else(|| value["text"].as_str().unwrap_or("").len() as u64);
    let original_error = value["error"].as_str().map(str::len);
    if value.to_string().len() <= MAX_REPLY_BYTES {
        return value;
    }
    value["resultTruncated"] = json!(true);
    value["textBytes"] = json!(original_text);
    if let Some(bytes) = original_error {
        value["errorBytes"] = json!(bytes);
    }
    while value.to_string().len() > MAX_REPLY_BYTES {
        let key = if value["text"].as_str().unwrap_or("").len()
            >= value["error"].as_str().unwrap_or("").len()
        {
            "text"
        } else {
            "error"
        };
        let text = value[key].as_str().unwrap_or("");
        let mut end = text.len() / 2;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        value[key] = json!(&text[..end]);
    }
    value
}

#[derive(Clone)]
pub(crate) enum Manager {
    Strong(Arc<AgentTools>),
    Weak(Weak<AgentTools>),
}
impl Manager {
    pub(crate) fn get(&self) -> Result<Arc<AgentTools>> {
        match self {
            Self::Strong(m) => Ok(m.clone()),
            Self::Weak(m) => m.upgrade().ok_or_else(|| anyhow!("agent host unavailable")),
        }
    }
}
struct AgentTool {
    manager: Manager,
    name: &'static str,
    definition: OnceLock<ToolDefinition>,
}
#[async_trait]
impl Tool for AgentTool {
    fn definition(&self) -> ToolDefinition {
        // Registration pins the definition to this registry snapshot. Live
        // reloads re-register controls; other turns keep their original schema.
        self.definition.get_or_init(|| {
            // A child's weak manager can be gone only during host teardown.
            let (limits, profiles) = match self.manager.get() {
                Ok(manager) => (manager.limits(), manager.profile_summaries()),
                Err(_) => (Agents::default(), Vec::new()),
            };
            // Registration compiles each input validator from this schema once. A
            // child registered while profiles are still being built therefore has
            // no profile enum; start still rejects unknown names.
            let mut profile_schema = json!({"type":"string","default":"read-only"});
            if !profiles.is_empty() {
                profile_schema["enum"] = json!(profiles.iter().map(|(n, _)| n).collect::<Vec<_>>());
            }
            let (mut description,mut schema)=match self.name {
                "agent"=>(format!("Start a reusable child session with explicit task/context and a registered profile. Returns handle and sessionId; wait or agent_status reads its answer. Default profile read-only. Depth limit {}; active limits {}/parent, {}/host. Profiles: {}",cap(limits.max_depth),cap(limits.max_active_per_parent),cap(limits.max_active_total),profiles.iter().map(|(n,d)| format!("{n}: {d}")).collect::<Vec<_>>().join("; ")),json!({"type":"object","properties":{"task":{"type":"string","minLength":1},"context":{},"profile":profile_schema},"required":["task"],"additionalProperties":false})),
                "agent_send"=>("Send text to an owned child by handle or sessionId. Steers running children immediately; starts a new turn in idle children.".into(),json!({"type":"object","properties":{"handle":{"type":"string"},"sessionId":{"type":"string"},"text":{"type":"string","minLength":1}},"required":["text"],"additionalProperties":false})),
                _=>("Report an owned child and consume its unread result, or list the descendant tree with parentSessionId and unread flags when no handle/sessionId is supplied. Listing does not consume results.".into(),json!({"type":"object","properties":{"handle":{"type":"string"},"sessionId":{"type":"string"}},"additionalProperties":false})),
            };
            if self.name == "agent" {
                schema["properties"]["worktree"] = json!({"type":"boolean","default":false});
                description.push_str(" worktree:true opts into a git worktree on bp/<sessionId> from the parent's current HEAD. Returns workspace, branch and worktree.path. Parent uncommitted changes are not copied. Default false.");
            }
            if self.name == "agent_send" {
                schema["properties"]["question_id"] = json!({"type":"string","minLength":1});
                description.push_str(" When answering a pending question, include question_id to correlate the answer. A question already answered by the user rejects a late answer with that id.");
            }
            if matches!(self.name, "agent" | "agent_send") {
                description.push_str(" Optional output_schema {name,schema} declares submit_result with that input schema; finish by calling it. Invalid arguments return a tool error; a missing submission gets one text-only reminder before failure. Completed results include json and text. max_tool_calls bounds this run, counting model calls and each nested tool call; child limits cannot exceed the calling turn's remaining budget or timeout. Omission inherits those bounds, otherwise unlimited. timeout is a nonnegative integer wall-clock limit in milliseconds, cancelling and cleaning up only this turn. Every finished child can receive a new turn with its full history. Follow-up budgets and timeouts are fresh; output_schema is retained unless replaced or cleared with null. Controls cannot be changed while steering an active turn.");
                schema["properties"]["output_schema"] = json!({"anyOf":[{"type":"object","properties":{"name":{"type":"string"},"schema":{"type":"object"}},"required":["name","schema"],"additionalProperties":false},{"type":"null"}]});
                schema["properties"]["max_tool_calls"] = json!({"type":"integer","minimum":0});
                schema["properties"]["timeout"] = json!({"type":"integer","minimum":0});
            }
            ToolDefinition {
                approval: Default::default(),
                name: self.name.into(),
                description,
                input_schema: schema,
                read_only: self.name == "agent_status",
            }
        }).clone()
    }
    async fn invoke(&self, context: ToolContext, input: Value) -> Result<ToolResult> {
        self.call(context, input, None)
    }
    async fn invoke_with_dispatcher(
        &self,
        context: ToolContext,
        input: Value,
        dispatcher: ToolDispatcher,
    ) -> Result<ToolResult> {
        self.call(context, input, Some(dispatcher))
    }
    fn cancel_session(&self, session: &str) -> usize {
        if self.name == "agent" {
            self.manager
                .get()
                .map(|m| m.cancel_session(session))
                .unwrap_or(0)
        } else {
            0
        }
    }
    fn take_reminders(&self, session: &str) -> Vec<bubblepaw_core::Reminder> {
        if self.name == "agent" {
            self.manager
                .get()
                .map(|manager| manager.take_reminders(session))
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    }
}
impl AgentTool {
    fn call(
        &self,
        context: ToolContext,
        input: Value,
        dispatcher: Option<ToolDispatcher>,
    ) -> Result<ToolResult> {
        let result = (|| {
            let manager = self.manager.get()?;
            match self.name {
                "agent" => manager
                    .start_owned(&context, input, dispatcher)
                    .map(|v| (v, true)),
                "agent_send" => manager.send_owned(&context, input, dispatcher),
                _ => manager
                    .status(&context.session_id, &input)
                    .map(|v| (v, false)),
            }
        })();
        // Admission rejection is a known no-effect outcome, so the dispatcher's
        // ordinary completion path must settle it, rather than mark it unknown.
        Ok(match result {
            Ok((v, true)) => ToolResult::pending(v.to_string()),
            Ok((v, false)) => ToolResult::text(v.to_string()),
            Err(e) => ToolResult::error(e.to_string()),
        })
    }
}

#[cfg(test)]
mod quota_resume_tests;

#[cfg(test)]
mod control_notice_tests;
