//! The rounds of one Messages reply that a length stop split.
use crate::{ModelResponse, QuotaReading, Usage};
use serde_json::{Value, json};

/// The rounds of one reply that a length stop split. A single round is
/// returned unchanged; several rounds join their text, usage and content.
#[derive(Default)]
pub(super) struct JoinedRounds {
    text: String,
    thinking: String,
    usage: Usage,
    request_usages: Vec<Usage>,
    content: Vec<Value>,
    rounds: usize,
    quota: Vec<QuotaReading>,
}

impl JoinedRounds {
    pub(super) fn push(&mut self, response: &ModelResponse) {
        self.quota.extend(response.quota.iter().cloned());
        self.rounds += 1;
        self.text.push_str(&response.text);
        self.thinking.push_str(&response.thinking);
        if let Some(usage) = &response.usage {
            self.usage.add(usage);
        }
        self.request_usages
            .extend(response.request_usages.iter().cloned());
        if let Some(blocks) = response
            .provider_state
            .as_ref()
            .and_then(|state| state["content"].as_array())
        {
            self.content.extend(blocks.iter().cloned());
        }
    }

    pub(super) fn finish(self, last: ModelResponse) -> ModelResponse {
        if self.rounds <= 1 {
            return last;
        }
        let stop_reason = last
            .provider_state
            .as_ref()
            .map(|state| state["stop_reason"].clone())
            .unwrap_or(Value::Null);
        ModelResponse {
            text: self.text,
            thinking: self.thinking,
            tool_calls: last.tool_calls,
            last_request_usage: last.last_request_usage,
            usage: Some(self.usage.clone()),
            request_usages: self.request_usages,
            model: last.model,
            stop_reason: last.stop_reason,
            quota: self.quota,
            provider_state: Some(json!({
                "adapter": "anthropic-messages",
                "content": self.content,
                "stop_reason": stop_reason,
                "cache_creation_input_tokens": self.usage.cache_creation_tokens,
            })),
            ..Default::default()
        }
    }
}
