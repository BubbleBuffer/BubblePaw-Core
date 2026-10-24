//! Run-owned structured completion and its single missing-submission reminder.
use super::*;

impl Harness {
    fn result_schema_notice(
        &self,
        session: &str,
        options: &RunOptions,
        tools: &ToolRegistry,
    ) -> Result<Option<String>> {
        let Some(mut surface) = self.store.chat_tool_surface(session)? else {
            return Ok(None);
        };
        let current = options
            .output_schema
            .as_ref()
            .and_then(|_| tools.get(crate::submit_result::NAME))
            .map(|tool| tool.definition());
        let differs =
            serde_json::to_value(&surface.initial_result)? != serde_json::to_value(&current)?;
        if differs && !surface.result_contract_changed {
            surface = self.store.update_chat_tool_surface(session, |surface| {
                surface.result_contract_changed = true;
                Ok(())
            })?;
        }
        if !differs && !surface.result_contract_changed {
            return Ok(None);
        }
        Ok(Some(match current {
            Some(definition) => format!("This run requires a structured result. Call submit_result directly or through call_tool/the tool wrapper using this current definition, even when the chat's frozen declaration differs or omits it:\n{}", serde_json::to_string(&definition)?),
            None => "This run has no output schema. submit_result is unavailable, even if its declaration remains in the chat prefix.".into(),
        }))
    }

    pub(super) fn queue_result_schema_notice(
        &self,
        run: &Run,
        options: &RunOptions,
        tools: &ToolRegistry,
    ) -> Result<()> {
        if let Some(text) = self.result_schema_notice(&run.session_id, options, tools)? {
            self.context_state().remind(
                &run.session_id,
                "root",
                &format!("result_schema:{}", run.id),
                &text,
            );
        }
        Ok(())
    }

    pub(super) fn refresh_result_schema_notice(
        &self,
        session: &str,
        options: &RunOptions,
        tools: &ToolRegistry,
    ) -> Result<()> {
        let Some(text) = self.result_schema_notice(session, options, tools)? else {
            return Ok(());
        };
        let run_id = self
            .state
            .active
            .lock()
            .unwrap()
            .get(session)
            .map(|active| active.run_id.clone());
        if let Some(run_id) = run_id {
            self.context_state().remind_fresh(
                session,
                "root",
                &format!("result_schema:{run_id}"),
                &text,
            );
        }
        Ok(())
    }
    pub(super) fn nudge_required_result(
        &self,
        run: &Run,
        message: &Message,
        request_usage: &mut Vec<RequestUsage>,
        nudged: &mut bool,
    ) -> Result<Option<String>> {
        self.state.log.settle_text(&run.session_id, || {
            self.store.append_with_request_usage(message, request_usage)
        })?;
        request_usage.clear();
        ensure!(
            !*nudged,
            "Required structured result missing: submit_result was not called after one host reminder"
        );
        self.context_state().remind(&run.session_id, "root", &format!("result_required:{}", run.id),
            "This run requires a structured result. Call submit_result with arguments matching its schema to finish. A text reply does not complete this run. This is the only reminder.");
        *nudged = true;
        let steered = self.store.consume(&run.session_id, true, false)?;
        if steered > 0 && self.config.auto_approval.is_some() {
            return self.latest_user_request(&run.session_id).map(Some);
        }
        Ok(None)
    }
}

pub(super) fn outcome(run: &Run, value: serde_json::Value) -> Result<TurnOutcome> {
    let text = serde_json::to_string(&value)?;
    let mut final_message = Store::message(&run.session_id, Role::Assistant, text.clone());
    final_message.origin = Some(crate::MessageOrigin::Generated);
    Ok(TurnOutcome {
        text,
        result_message_id: final_message.id.clone(),
        final_message: Some(final_message),
    })
}

pub(super) fn text_outcome(message: Message, needs_commit: bool) -> TurnOutcome {
    TurnOutcome {
        text: message.content.clone(),
        result_message_id: message.id.clone(),
        final_message: needs_commit.then_some(message),
    }
}
