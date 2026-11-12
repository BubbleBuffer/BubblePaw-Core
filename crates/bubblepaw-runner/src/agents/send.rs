//! Sending to a child, including resuming a quota wait.
use super::*;

impl AgentTools {
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
    pub(super) fn send_owned(
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
            controlled_prompt(text, &controls, &child.harness, &child.session_id)?
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
}
