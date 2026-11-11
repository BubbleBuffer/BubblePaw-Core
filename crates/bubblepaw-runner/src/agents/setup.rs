//! Construction, builders and profile registration for the agent tools.
use super::*;

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
    pub(super) fn profiles(&self) -> &[AgentProfile] {
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
    pub(super) fn limits(&self) -> Agents {
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
    pub(super) fn live_child_approval(&self, harness: &Arc<Harness>) -> bool {
        self.registry.live_approval.load(Ordering::Acquire)
            && self
                .profiles()
                .iter()
                .any(|profile| Arc::ptr_eq(&profile.harness, harness))
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn child_run_options(
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
    pub fn profile(&self, name: &str) -> Option<&AgentProfile> {
        self.profile_enabled(name)
            .then(|| self.stored_profile(name))
            .flatten()
    }
    pub(super) fn stored_profile(&self, name: &str) -> Option<&AgentProfile> {
        self.profiles().iter().find(|p| p.name == name)
    }
    pub(super) fn profile_enabled(&self, name: &str) -> bool {
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
}
