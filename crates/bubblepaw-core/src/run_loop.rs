//! One run of the loop: rounds of model request and tool dispatch until the run
//! has an outcome. The shell in `runtime.rs` owns the session around it.
use super::run_settle::{RoundState, Settled};
use super::tool_source::{RunTools, ToolSource};
use super::*;

/// What one run needs to start: the run, its options and how it is stopped.
#[derive(Clone, Copy)]
pub(super) struct RunInputs<'a> {
    pub(super) run: &'a Run,
    pub(super) options: &'a RunOptions,
    pub(super) cancellation: &'a CancellationToken,
    pub(super) single_tool: Option<&'a SingleToolRun>,
    /// The tools each round offers.
    pub(super) tools: &'a dyn ToolSource,
    /// Decides whether a call may run, and shapes its result.
    pub(super) gate: &'a Arc<dyn ToolGate>,
    /// Whether the gate is told the user's latest request, kept current as steering arrives.
    pub(super) wants_user_request: bool,
}

/// How a run ended, for the shell to record on the run and commit.
pub(super) struct RunOutcome {
    pub(super) status: String,
    pub(super) text: Option<String>,
    pub(super) usage: Option<Usage>,
    pub(super) error: Option<String>,
    pub(super) result_message_id: Option<String>,
    pub(super) final_message: Option<Message>,
}

impl Harness {
    /// Run exactly one run. A failure keeps the partial reply and closes the run's
    /// open records through the journal before the outcome is returned.
    pub(super) async fn run_once(&self, inputs: &RunInputs<'_>) -> RunOutcome {
        let run = inputs.run;
        let mut usage = None;
        let partial = runtime_stream::SharedPartial::default();
        match self.turn(inputs, &mut usage, &partial).await {
            Ok(outcome) => RunOutcome {
                status: "completed".into(),
                text: Some(outcome.text),
                usage: Some(usage.unwrap_or_default()),
                error: None,
                result_message_id: Some(outcome.result_message_id),
                final_message: outcome.final_message,
            },
            Err(error) => {
                let status = if inputs.cancellation.is_cancelled() {
                    "cancelled"
                } else {
                    "failed"
                };
                let mut message = format!("{error:#}");
                self.keep_partial_reply(
                    &run.session_id,
                    &partial,
                    inputs.cancellation.is_cancelled(),
                );
                if let Err(storage_error) = self.journal.run_closed(&run.session_id, &run.id) {
                    message = format!("{error:#}; recovery failed: {storage_error}");
                }
                RunOutcome {
                    status: status.into(),
                    text: None,
                    usage,
                    error: Some(message),
                    result_message_id: None,
                    final_message: None,
                }
            }
        }
    }
    pub(super) fn latest_user_request(&self, session_id: &str) -> Result<String> {
        let is_user_request = |message: &Message| {
            message.role == Role::User && message.origin != Some(crate::MessageOrigin::Generated)
        };
        let history = self.request_context.history(session_id)?;
        if let Some(message) = history.into_iter().rev().find(is_user_request) {
            return Ok(message.text().into_owned());
        }
        Ok(self
            .request_context
            .context_prefix(session_id)?
            .into_iter()
            .rev()
            .find(is_user_request)
            .map(|message| message.text().into_owned())
            .unwrap_or_default())
    }
    async fn turn(
        &self,
        inputs: &RunInputs<'_>,
        usage: &mut Option<Usage>,
        partial: &runtime_stream::SharedPartial,
    ) -> Result<TurnOutcome> {
        let RunInputs {
            run,
            options,
            cancellation,
            single_tool,
            tools: tool_source,
            gate,
            wants_user_request,
        } = *inputs;
        // The host's view of the run's turn, for as long as this future lives.
        let _turn = self.request_context.turn_scope(run);
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
        let mut state = RoundState {
            user_request: if wants_user_request {
                self.latest_user_request(&run.session_id)?
            } else {
                String::new()
            },
            ..Default::default()
        };
        let RunTools {
            mut tools,
            surface: mut tool_surface,
            refresh_each_round,
        } = tool_source.open_run(run, options)?;
        let mut dispatcher = ToolDispatcher::from_seams(
            tools.clone(),
            self.config.max_tool_result_bytes,
            self.config.observation.clone(),
            self.journal.clone(),
            gate.clone(),
            self.live.clone(),
        )
        .with_approval(self.config.approval.clone())
        .with_approval_resolver(
            self.config
                .run_approval_resolver(options.approval.clone(), single_tool.is_some()),
        )
        .with_tool_budget(options.max_tool_calls)
        .with_interactive_approval(single_tool.is_none());
        let max_rounds = options.max_rounds.or(self.config.max_rounds);
        let mut budget_final_requested = false;
        // Exhaustion gets a final tools-off request; a missing result gets one nudge.
        // An ignored tools-off reminder gets one enforcement round after them.
        let extra_rounds = usize::from(options.output_schema.is_some())
            + usize::from(options.max_tool_calls.is_some())
            + 1;
        let mut rounds = 0usize;
        let mut reminders = runtime_reminders::RunReminders::default();
        let mut overflow = runtime_overflow::OverflowRetry::default();
        loop {
            let round = rounds;
            rounds = rounds.saturating_add(1);
            let refresh = dispatcher.take_tools_refresh();
            if refresh_each_round || refresh || round == 0 {
                tools = tool_source.round_tools(run, options)?;
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
                            || state.result_retry_pending
                            || budget_final_round
                            || enforcing)
                }),
                "Maximum model/tool rounds reached"
            );
            budget_final_requested |= budget_final_round;
            ensure!(!cancellation.is_cancelled(), "Cancelled");
            let mut dispatcher = dispatcher
                .clone()
                .with_user_request(state.user_request.clone());
            let mut tools_round = runtime_reminders::ToolsRound::Open;
            let mut declarations = None;
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
                tools_round = tool_source.queue_round_reminders(
                    run,
                    options,
                    dispatcher.remaining_tool_calls(),
                    &mut reminders,
                );
                for reminder in tools.take_reminders(&run.session_id) {
                    self.request_context
                        .note(&run.session_id, Note::Reminder(reminder));
                }
                let mut history = self.request_context.history(&run.session_id)?;
                let request_scope = self.config.observation.as_ref().map(|_| ObservationScope {
                    session_id: run.session_id.clone(),
                    run_id: run.id.clone(),
                    model_request_id: Some(id()),
                    ..Default::default()
                });
                let mut late_steering = false;
                let mut steering_batch = Vec::<Message>::new();
                let mut context_notices = Vec::<String>::new();
                let mut preparation_mode = overflow.mode();
                let mut rejected_frontier = None;
                let prepared = async {
                    loop {
                    let system_prompt = tool_surface.system_prompt.clone();
                    let Draft { messages, preview } =
                        self.request_context.draft(&run.session_id, &history, &steering_batch)?;
                    let request = ModelRequest {
                        model: options.model.clone().unwrap_or_else(|| default_model.clone()),
                        system_prompt,
                        messages,
                        tools: tool_surface.declarations.clone(),
                        max_output_tokens: options.max_output_tokens,
                        ..Default::default()
                    };
                    let mut prepared = tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => Err(anyhow!("Cancelled during context preparation")),
                        result = self.request_context.prepare(&run.session_id, request, request_provider.clone(), preparation_mode, cancellation.clone()) => result,
                    }?;
                    // Preparation may have made physical auxiliary calls even
                    // when a late batch requires a fresh request projection.
                    if let Some(value) = prepared.usage.take() {
                        usage.get_or_insert_with(Usage::default).add(&value);
                    }
                    if let Some(notice) = prepared.notice.take() {
                        context_notices.push(notice);
                    }
                    if tool_source.surface_changed(run, options, &mut tools, &mut tool_surface, &mut prepared.request)? {
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
                    let steers = self.journal.steering_batch_consumed(&run.session_id)?;
                    if !steers.is_empty() {
                        late_steering = true;
                        steering_batch.extend(steers);
                        history = self.request_context.history(&run.session_id)?;
                        continue;
                    }
                    let updated = self.request_context.reminder_preview(&run.session_id)?;
                    if preview.as_ref().map(|m| &m.content) != updated.as_ref().map(|m| &m.content) {
                        continue;
                    }
                    if let Some(preview) = &preview {
                        prepared.request.messages.retain(|saved| saved.id != preview.id);
                    }
                    prepared.request = match self.request_context.build(&run.session_id, prepared.request, request_scope.as_ref(), request_provider.clone())? {
                        Built::Ready(request) => *request,
                        Built::TooLarge(budget) => {
                            let frontier = budget.frontier;
                            // One attempt is allowed without an applied view. Every
                            // later recovery must have advanced complete coverage.
                            if rejected_frontier.is_some_and(|previous| previous >= frontier) {
                                return Err(budget.error);
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
                            history = self.request_context.history(&run.session_id)?;
                            continue;
                        }
                    };
                    overflow.check_smaller(&prepared.request)?;
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
                let prepared = prepared.map_err(|error| overflow.failed(error))?;
                if late_steering && wants_user_request {
                    state.user_request = self.latest_user_request(&run.session_id)?;
                    dispatcher = dispatcher.with_user_request(state.user_request.clone());
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
                    self.live.transient(
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
                let request_started = Instant::now();
                let first_text = self
                    .config
                    .observation
                    .as_ref()
                    .map(|_| Arc::new(Mutex::new(None::<u64>)));
                self.request_context.model_started(
                    run,
                    request_scope.as_ref(),
                    &request,
                    &request_provider,
                );
                let dispatcher = dispatcher.with_request(
                    request_scope
                        .as_ref()
                        .and_then(|s| s.model_request_id.clone()),
                );
                partial.lock().unwrap().clear();
                let paragraphs = Arc::new(Mutex::new(Paragraphs::default()));
                let max = self.config.max_paragraph_bytes.max(4);
                let callback_live = Arc::new(AtomicBool::new(true));
                let callback_lifetime = CallbackLifetime(callback_live.clone());
                let callback = runtime_stream::ModelStream {
                    // A provider retaining the callback must not retain the archive or engine.
                    events: self.live.clone(),
                    session: run.session_id.clone(),
                    run_id: run.id.clone(),
                    live: callback_live,
                    cancellation: cancellation.clone(),
                    paragraphs: paragraphs.clone(),
                    max_paragraph_bytes: max,
                    started: request_started,
                    first_text: first_text.clone(),
                    partial: partial.clone(),
                }
                .callback();
                self.live
                    .transient(&run.session_id, &run.id, EventKind::ModelRequestStarted {});
                self.request_context.request_sent(
                    &run.session_id,
                    &request,
                    request_provider.clone(),
                );
                // Check direct calls against the declarations actually sent this round.
                declarations = Some(
                    request
                        .tools
                        .iter()
                        .map(|tool| tool.name.clone())
                        .collect::<Vec<_>>(),
                );
                overflow.sent(&request);
                let mut response_result = self
                    .request_context
                    .requester(run, request_provider.clone())
                    .generate(request, callback, cancellation.clone())
                    .await;
                drop(callback_lifetime);
                self.request_context.model_finished(
                    run,
                    request_scope.as_ref(),
                    &mut response_result,
                    (
                        request_started.elapsed().as_millis() as u64,
                        first_text.as_ref().and_then(|v| *v.lock().unwrap()),
                    ),
                    cancellation.is_cancelled(),
                );
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
                        overflow.arm(error)?;
                        rounds = round;
                        continue;
                    }
                };
                overflow.reset();
                partial.lock().unwrap().clear();
                state.result_retry_pending = false;
                reminders.observe_tool_calls(tools_round, response.tool_calls.len());
                self.request_context
                    .response_received(&run.session_id, &response);
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
                        self.live.transient(
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
            let settled = self
                .settle_reply(
                    inputs,
                    &mut state,
                    response,
                    &dispatcher,
                    tools_round,
                    declarations.as_deref(),
                )
                .await?;
            match settled {
                Settled::Retry => continue,
                Settled::Done(outcome) => return Ok(*outcome),
                Settled::Next => {}
            }
        }
    }
}
