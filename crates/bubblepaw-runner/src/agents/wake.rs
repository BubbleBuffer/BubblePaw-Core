//! Child wake registration and wake-started child turns.
use super::*;

impl AgentTools {
    pub(super) fn workspace_runtime(
        &self,
        session: &str,
        profile: &str,
    ) -> Result<Option<Arc<ChildRuntime>>> {
        let Some(host) = self.registry.workspace_host.get() else {
            return Ok(None);
        };
        let workspace = crate::worktrees::workspace(&self.store, session, &host.root)?;
        if workspace == host.root {
            return Ok(None);
        }
        Ok(Some(Arc::new((host.factory)(&workspace, profile)?)))
    }
    pub(super) fn admit_scoped<T>(
        &self,
        session: &str,
        start: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        match &self.registry.scope {
            Some(scope) => scope.admit_owned(&self.store, session, start),
            None => start(),
        }
    }
    pub(super) fn register_child_wake(&self, child: &Arc<Child>) {
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
    pub(super) fn start_wake_child_input(
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
}
