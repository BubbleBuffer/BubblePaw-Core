//! The typed reply facts each adapter reads from its provider's reply: the model
//! the provider says it used and why the final request stopped. Each function
//! returns a reply carrying only those facts, for the adapter to spread into the
//! reply it builds; a provider that reports nothing leaves them `None`.
use crate::{ModelResponse, StopReason, Usage};
use serde_json::Value;

fn reported_model(value: &Value) -> Option<String> {
    value["model"]
        .as_str()
        .filter(|model| !model.is_empty())
        .map(str::to_owned)
}

fn other(raw: &str) -> StopReason {
    StopReason::Other(raw.to_owned())
}

/// A chat completions payload: `finish_reason` of the first choice.
pub(super) fn chat(payload: &Value) -> ModelResponse {
    let stop_reason = payload["choices"][0]["finish_reason"]
        .as_str()
        .map(|reason| match reason {
            "stop" => StopReason::EndTurn,
            "tool_calls" | "function_call" => StopReason::ToolUse,
            "length" => StopReason::MaxOutputTokens,
            "content_filter" => StopReason::ContentFilter,
            raw => other(raw),
        });
    ModelResponse {
        model: reported_model(payload),
        stop_reason,
        ..Default::default()
    }
}

/// An Anthropic messages payload: `stop_reason`.
pub(super) fn messages(payload: &Value) -> ModelResponse {
    let stop_reason = payload["stop_reason"].as_str().map(|reason| match reason {
        "end_turn" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxOutputTokens,
        "refusal" => StopReason::ContentFilter,
        raw => other(raw),
    });
    ModelResponse {
        model: reported_model(payload),
        stop_reason,
        ..Default::default()
    }
}

/// A finished Responses object: its status, incomplete reason and whether the
/// reply carries tool calls.
pub(super) fn responses(response: &Value, has_calls: bool) -> ModelResponse {
    let stop_reason = match response["status"].as_str() {
        Some("completed") if has_calls => Some(StopReason::ToolUse),
        Some("completed") => Some(StopReason::EndTurn),
        Some("incomplete") => Some(match response["incomplete_details"]["reason"].as_str() {
            Some("max_output_tokens") => StopReason::MaxOutputTokens,
            Some("content_filter") => StopReason::ContentFilter,
            Some(raw) => other(raw),
            None => other("incomplete"),
        }),
        Some(raw) => Some(other(raw)),
        None => None,
    };
    ModelResponse {
        model: reported_model(response),
        stop_reason,
        ..Default::default()
    }
}

/// The usage block of a finished Responses object, when it carries one.
pub(super) fn responses_usage(response: &Value) -> Option<Usage> {
    response
        .get("usage")
        .filter(|v| v.is_object())
        .map(|v| Usage {
            input_tokens: v["input_tokens"].as_u64().unwrap_or(0),
            output_tokens: v["output_tokens"].as_u64().unwrap_or(0),
            cache_read_tokens: v["input_tokens_details"]["cached_tokens"]
                .as_u64()
                .unwrap_or(0),
            cache_creation_tokens: v["input_tokens_details"]["cache_creation_tokens"]
                .as_u64()
                .or_else(|| v["cache_creation_input_tokens"].as_u64())
                .unwrap_or(0),
            reasoning_tokens: v["output_tokens_details"]["reasoning_tokens"].as_u64(),
        })
}
