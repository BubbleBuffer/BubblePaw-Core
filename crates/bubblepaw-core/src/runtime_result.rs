//! Run-owned structured completion and its single missing-submission reminder.
use super::*;
use crate::ContentBlock;

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
            self.request_context.note(
                &run.session_id,
                Note::Text {
                    kind: &format!("result_schema:{}", run.id),
                    text: &text,
                },
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
            self.request_context.note(
                session,
                Note::Fresh {
                    kind: &format!("result_schema:{run_id}"),
                    text: &text,
                },
            );
        }
        Ok(())
    }
    /// Bound a tool result to the configured size and append the budget footer.
    pub(super) fn bound_tool_result(
        &self,
        result: ToolResult,
        remaining_calls: Option<usize>,
    ) -> ToolResult {
        let footer = remaining_calls
            .map(|remaining| format!("\n\n{remaining} tool calls left"))
            .unwrap_or_default();
        let limit = self
            .config
            .max_tool_result_bytes
            .saturating_sub(footer.len());
        let mut result = result;
        result.bound_text(limit);
        if !footer.is_empty() {
            // After the last block: the text reads body then footer, and an image
            // block never sits inside either.
            result.content.push(ContentBlock::Text { text: footer });
        }
        result
    }
    /// The run requires a structured result and the model replied without it.
    pub(super) fn require_result(&self, run: &Run, nudged: &mut bool) -> Result<()> {
        ensure!(
            !*nudged,
            "Required structured result missing: submit_result was not called after one host reminder"
        );
        self.request_context.note(
            &run.session_id,
            Note::Text {
                kind: &format!("result_required:{}", run.id),
                text:
            "This run requires a structured result. Call submit_result with arguments matching its schema to finish. A text reply does not complete this run. This is the only reminder.",
            },
        );
        *nudged = true;
        Ok(())
    }
}
