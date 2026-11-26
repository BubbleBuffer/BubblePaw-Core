//! The second half of a round: checks on the model reply, its recording, tool
//! dispatch and the steering that follows.
use crate::dispatch::ToolDispatcher;
use crate::loop_records::{self, TurnOutcome};
use crate::round_reminders::ToolsRound;
use crate::run_inputs::{RunInputs, RunLoop};
use crate::run_journal::{Consumed, InputScope};
use crate::submit_result::is_submission;
use anyhow::{Result, anyhow, ensure};
use bubblepaw_ai::{Message, ModelResponse, Role, ToolCall, reply_blocks};

/// Loop state that outlives one round and is shared with the settling step.
#[derive(Default)]
pub(crate) struct RoundState {
    pub(crate) user_request: String,
    pub(crate) result_nudged: bool,
    pub(crate) result_retry_pending: bool,
    pub(crate) malformed_replies: usize,
}

/// What a settled round asks of the loop.
pub(crate) enum Settled {
    /// Ask the model again.
    Retry,
    /// The run has its outcome.
    Done(Box<TurnOutcome>),
    /// The round is complete; go on to the next one.
    Next,
}

/// Replies made only of malformed tool calls, in a row, end the run.
pub(crate) fn check_malformed_reply_limit(streak: &mut usize, all_malformed: bool) -> Result<()> {
    const MAX_MALFORMED_REPLIES: usize = 3;
    *streak = if all_malformed { *streak + 1 } else { 0 };
    ensure!(
        *streak < MAX_MALFORMED_REPLIES,
        "Three consecutive model replies contained only malformed tool calls"
    );
    Ok(())
}

impl Settled {
    fn done(outcome: TurnOutcome) -> Self {
        Self::Done(Box::new(outcome))
    }
}

impl RunLoop {
    pub(crate) async fn settle_reply(
        &self,
        inputs: &RunInputs<'_>,
        state: &mut RoundState,
        response: ModelResponse,
        dispatcher: &ToolDispatcher,
        tools_round: ToolsRound,
        declarations: Option<&[String]>,
    ) -> Result<Settled> {
        let RunInputs {
            run,
            options,
            cancellation,
            single_tool,
            tools: tool_source,
            wants_user_request,
            ..
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
            response.storage_bytes()? <= self.limits.max_request_bytes,
            "Provider response exceeds configured storage size limit"
        );
        let mut message = Message::new(&run.session_id, Role::Assistant, response.text.clone());
        message.content = reply_blocks(&response.thinking, &response.text);
        message.tool_calls = response.tool_calls.clone();
        message.provider_state = response.provider_state;
        // The ordinary final response, result pointer, run state and its two
        // events are one commit in the shell. Tool rounds still checkpoint
        // messages before crossing any dispatch boundary, where the gate may ask the user.
        if options.script.is_none()
            && response.tool_calls.is_empty()
            && options.output_schema.is_some()
        {
            state.malformed_replies = 0;
            self.journal.reply_received(&run.id, &message)?;
            tool_source.result_required(run, &mut state.result_nudged)?;
            let steered = self
                .journal
                .inputs_consumed(&run.session_id, Consumed::Steering)?;
            if steered > 0 && wants_user_request {
                state.user_request = self.latest_user_request(&run.session_id)?;
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
            return Ok(Settled::done(loop_records::text_outcome(message, true)));
        }
        // A sole Allow call batches its reply and dispatch marker.
        let prepare = |call: &ToolCall| {
            dispatcher.prepare_call(
                dispatcher.call_context(run, cancellation),
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
            let result =
                tool_source.bound_result(dispatched.result, dispatcher.remaining_tool_calls());
            let mut result_message = Message::new(&run.session_id, Role::Tool, String::new());
            result_message.content = result.content.clone();
            let captured_result = single_tool.map(|_| result.clone());
            result_message.tool_call_id = Some(call.id.clone());
            result_message.is_error = result.is_error;
            result_message.details = result.details.clone();
            if result.terminate && terminated.is_none() {
                terminated = Some(loop_records::terminated(&result_message));
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
        check_malformed_reply_limit(&mut state.malformed_replies, all_malformed)?;
        if let Some(value) = dispatcher.take_submitted_result() {
            return loop_records::outcome(run, value).map(Settled::done);
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
        if steered > 0 && wants_user_request {
            state.user_request = self.latest_user_request(&run.session_id)?;
        }
        if options.output_schema.is_none() && response.tool_calls.is_empty() && steered == 0 {
            return Ok(Settled::done(loop_records::text_outcome(message, false)));
        }
        Ok(Settled::Next)
    }
}
