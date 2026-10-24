//! Fork-local return envelopes. These targets never enter the tool registry.
use super::*;
use bubblepaw_core::ModelResponse;

#[derive(Clone, Copy)]
pub(crate) struct ForkDelivery {
    target: &'static str,
    native: bool,
}

impl ForkDelivery {
    pub fn new(request: &ModelRequest, target: &'static str) -> Self {
        Self {
            target,
            native: request.tools.iter().any(|tool| tool.name == "call_tool"),
        }
    }

    pub fn instruction(self) -> String {
        if self.native {
            format!(
                "No workspace tool execution is admitted in this branch. Return the complete memory text using only call_tool({{name:'{}',arguments:{{summary:'text'}}}}). This return envelope overrides prose-only delivery rules; the summary string carries the existing requested heading, prose and any final Related line. This synthetic return target exists only in this branch; do not discover it or call any other tool. The inherited submit_result tool belongs to the main task: never call it directly, through call_tool or through js, and do not shape memory as the main task's result.",
                self.target
            )
        } else {
            "No tool execution is admitted in this branch. Do not call tools, including the main task's submit_result directly or through js. Answer in text."
                .into()
        }
    }

    /// Consume exactly one branch-local delivery call. Everything else stays a
    /// tool call and follows the ordinary refusal/retry path, without execution.
    pub fn extract(self, response: &mut ModelResponse) -> bool {
        if response.tool_calls.is_empty() {
            return true;
        }
        if !self.native || response.tool_calls.len() != 1 {
            return false;
        }
        let call = &response.tool_calls[0];
        let Some(envelope) = call.arguments.as_object() else {
            return false;
        };
        let Some(arguments) = envelope.get("arguments").and_then(Value::as_object) else {
            return false;
        };
        let Some(summary) = arguments.get("summary").and_then(Value::as_str) else {
            return false;
        };
        if call.name != "call_tool"
            || envelope.len() != 2
            || envelope.get("name").and_then(Value::as_str) != Some(self.target)
            || arguments.len() != 1
        {
            return false;
        }
        response.text = summary.to_owned();
        response.tool_calls.clear();
        true
    }

    pub fn refusal(self) -> String {
        if self.native {
            self.instruction()
        } else {
            super::TOOL_REFUSAL.into()
        }
    }
}
