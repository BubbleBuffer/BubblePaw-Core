//! Starting child agents and workers, with depth and slot limits.
use super::*;

impl AgentTools {
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
    pub(super) fn inherited_controls(
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
    pub(super) fn depth(&self, session: &str) -> Result<usize> {
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
    pub(super) fn check_slots(&self, state: &State, owner: &str) -> Result<()> {
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
    pub(super) fn retain_slot(&self, state: &mut State) -> Result<()> {
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
    pub(super) fn start_owned(
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
        // The default cap is 3; zero disables spawning.
        if let Some(max) = self.limits().max_depth {
            ensure!(depth <= max, "subagent depth limit {max} reached");
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
                controlled_prompt(&prompt, &controls, &profile.harness, &session.id)?,
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
}
