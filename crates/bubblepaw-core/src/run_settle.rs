//! The second half of a round: checks on the model reply, its recording, tool
//! dispatch and the steering that follows.
use super::run_loop::RunInputs;
use super::*;

/// Loop state that outlives one round and is shared with the settling step.
#[derive(Default)]
pub(super) struct RoundState {
    pub(super) user_request: String,
    pub(super) result_nudged: bool,
    pub(super) result_retry_pending: bool,
    pub(super) malformed_replies: usize,
}

/// What a settled round asks of the loop.
pub(super) enum Settled {
    /// Ask the model again.
    Retry,
    /// The run has its outcome.
    Done(Box<TurnOutcome>),
    /// The round is complete; go on to the next one.
    Next,
}

impl Settled {
    fn done(outcome: TurnOutcome) -> Self {
        Self::Done(Box::new(outcome))
    }
}

impl Harness {
    pub(super) async fn settle_reply(
        &self,
        inputs: &RunInputs<'_>,
        state: &mut RoundState,
        response: ModelResponse,
        dispatcher: &ToolDispatcher,
        tools_round: runtime_reminders::ToolsRound,
        declarations: Option<&[String]>,
    ) -> Result<Settled> {
        let RunInputs {
            run,
            options,
            cancellation,
            single_tool,
        } = *inputs;
        // Reissuing a durable call ID is never a new invocation, including after
        // an unknown outcome. The host must resolve that effect explicitly.
        let mut call_ids = std::collections::HashSet::new();
        for call in &response.tool_calls {
            ensure!(
                !call.id.is_empty()
                    && call_ids.insert(call.id.clone())
                    && !self.journal.tool_call_recorded(&run.session_id, &call.id)?,
                "Provider returned duplicate, reused, or empty tool call IDs"
            );
        }
        ensure!(
            response.storage_bytes()? <= self.config.max_request_bytes,
            "Provider response exceeds configured storage size limit"
        );
        let mut message = Message::new(&run.session_id, Role::Assistant, response.text.clone());
        message.content = crate::reply_blocks(&response.thinking, &response.text);
        message.tool_calls = response.tool_calls.clone();
        message.provider_state = response.provider_state;
        // The ordinary final response, result pointer, run state and its two
        // events are one commit in the shell. Tool rounds still checkpoint
        // messages before crossing any dispatch/approval boundary.
        if options.script.is_none()
            && response.tool_calls.is_empty()
            && options.output_schema.is_some()
        {
            state.malformed_replies = 0;
            if let Some(request) =
                self.nudge_required_result(run, &message, &mut state.result_nudged)?
            {
                state.user_request = request;
            }
            state.result_retry_pending = true;
            return Ok(Settled::Retry);
        }
        if options.script.is_none()
            && response.tool_calls.is_empty()
            && !self
                .journal
                .input_pending(&run.session_id, InputScope::Steering)?
        {
            return Ok(Settled::done(runtime_result::text_outcome(message, true)));
        }
        // A sole Allow call batches its reply and dispatch marker.
        let prepare = |call: &ToolCall| {
            dispatcher.prepare_call(
                ToolContext {
                    session_id: run.session_id.clone(),
                    run_id: run.id.clone(),
                    invocation_id: String::new(),
                    cancellation: cancellation.clone(),
                    approval: Default::default(),
                },
                call.clone(),
                tools_round.is_open(),
                declarations,
            )
        };
        let mut single_prepared = match response.tool_calls.as_slice() {
            [call] => Some(prepare(call)?),
            _ => None,
        };
        let mut prepared_invocation = if options.script.is_none()
            && let Some(prepared) = &mut single_prepared
            && dispatcher.admit_fast_path(prepared)?
        {
            Some(dispatcher.commit_reply_dispatch(&message, prepared)?)
        } else {
            self.journal.reply_received(&run.id, &message)?;
            None
        };
        // Native submissions settle last. Calls retain their provider order
        // in history; unrelated effects run before accepting the result.
        let calls = &response.tool_calls;
        let ordered_calls = calls
            .iter()
            .filter(|call| !is_submission(call))
            .chain(calls.iter().filter(|call| is_submission(call)));
        let mut all_malformed = !calls.is_empty();
        let mut terminated = None;
        for call in ordered_calls {
            let prepared = match single_prepared.take() {
                Some(prepared) => prepared,
                None => prepare(call)?,
            };
            all_malformed &= prepared.malformed;
            let dispatched = dispatcher
                .invoke_direct(prepared, &message.id, prepared_invocation.take())
                .await?;
            let result = self.bound_tool_result(dispatched.result, dispatcher);
            let mut result_message = Message::new(&run.session_id, Role::Tool, String::new());
            result_message.content = result.content.clone();
            let captured_result = single_tool.map(|_| result.clone());
            result_message.tool_call_id = Some(call.id.clone());
            result_message.is_error = result.is_error;
            result_message.details = result.details.clone();
            if result.terminate && terminated.is_none() {
                terminated = Some(runtime_result::terminated(&result_message));
            }
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
        runtime_tools::check_malformed_reply_limit(&mut state.malformed_replies, all_malformed)?;
        if let Some(value) = dispatcher.take_submitted_result() {
            return runtime_result::outcome(run, value).map(Settled::done);
        }
        if let Some(outcome) = terminated {
            return Ok(Settled::done(outcome));
        }
        if options.script.is_some() || single_tool.is_some() {
            if single_tool.is_none() {
                self.journal
                    .inputs_consumed(&run.session_id, Consumed::GeneratedSteering)?;
            }
            let history = self.request_context.history(&run.session_id)?;
            let result_message_id = history
                .iter()
                .rev()
                .find(|message| {
                    message.tool_call_id.as_deref() == Some(response.tool_calls[0].id.as_str())
                })
                .map(|message| message.id.clone())
                .ok_or_else(|| anyhow!("Script result was not recorded"))?;
            return crate::script::outcome(&history, &response.tool_calls).map(|text| {
                Settled::done(TurnOutcome {
                    text,
                    result_message_id,
                    final_message: None,
                })
            });
        }
        // Consume steering after a complete model/tool round. The final
        // preparation drain also catches admissions arriving after this.
        let steered = self
            .journal
            .inputs_consumed(&run.session_id, Consumed::Steering)?;
        if steered > 0 && self.config.auto_approval.is_some() {
            state.user_request = self.latest_user_request(&run.session_id)?;
        }
        if options.output_schema.is_none() && response.tool_calls.is_empty() && steered == 0 {
            return Ok(Settled::done(runtime_result::text_outcome(message, false)));
        }
        Ok(Settled::Next)
    }
}
