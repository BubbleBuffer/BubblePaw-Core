//! Model-less script runs: one synthesized `js` round in place of a provider call.
use crate::ids::id;
use anyhow::{Result, anyhow};
use bubblepaw_ai::{Message, ModelResponse, Role, ToolCall};
use serde_json::json;

/// The `model` recorded on a script run.
pub const SCRIPT_MODEL: &str = "script";

/// The assistant turn a model would have produced to run `code`.
pub fn response(code: &str) -> ModelResponse {
    ModelResponse {
        text: String::new(),
        thinking: String::new(),
        tool_calls: vec![ToolCall {
            id: format!("script_{}", id()),
            name: "js".into(),
            arguments: json!({ "code": code }),
        }],
        provider_state: Some(json!({ "script": true })),
        ..Default::default()
    }
}

/// The run's final text is the stored tool result; a failed cell fails the run.
pub fn outcome(history: &[Message], calls: &[ToolCall]) -> Result<String> {
    let call = &calls[0].id;
    let result = history
        .iter()
        .rev()
        .find(|message| message.role == Role::Tool && message.tool_call_id.as_ref() == Some(call))
        .ok_or_else(|| anyhow!("Script result was not recorded"))?;
    if result.is_error {
        Err(anyhow!("{}", result.text()))
    } else {
        Ok(result.text().into_owned())
    }
}
