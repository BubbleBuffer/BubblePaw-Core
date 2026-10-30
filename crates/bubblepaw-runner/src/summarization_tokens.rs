//! Canonical token estimation and bounded message-content memoization.
use super::*;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;

/// Estimate one canonical message using the runner's ordinary serving estimator.
/// Responses replay items are hydrated from canonical text and calls; opaque
/// encrypted reasoning is excluded. This is an estimate, never provider usage.
pub fn canonical_message_tokens(message: &Message) -> usize {
    let body = if let Some(state) = &message.provider_state
        && state["adapter"] == "responses"
        && state["items"].is_array()
    {
        let mut items = json!(
            crate::provider::replay_state::hydrate(state, &message.content, &message.tool_calls)
                .unwrap_or_else(|_| state["items"].as_array().unwrap().clone())
        );
        for item in items.as_array_mut().unwrap() {
            if let Some(object) = item.as_object_mut() {
                object.remove("encrypted_content");
            }
        }
        items.to_string()
    } else {
        json!({"role":message.role,"content":message.content,"tool_calls":message.tool_calls,"tool_call_id":message.tool_call_id}).to_string()
    };
    crate::episodes::text_tokens(&body) + 4
}

#[derive(Default)]
pub(super) struct TokenCache {
    pub(super) counts: HashMap<[u8; 32], usize>,
    order: VecDeque<[u8; 32]>,
    #[cfg(test)]
    pub(super) computations: usize,
}

// Count by message contents, including rendered context messages whose generated
// IDs can change on every preparation. Bound disposable entries in a long-lived host.
impl TokenCache {
    pub(super) fn insert(&mut self, key: [u8; 32], count: usize) {
        const CAPACITY: usize = 65_536;
        if self.counts.insert(key, count).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > CAPACITY {
            if let Some(oldest) = self.order.pop_front() {
                self.counts.remove(&oldest);
            }
        }
    }
}

fn hash_text(hasher: &mut Sha256, text: &str) {
    hasher.update((text.len() as u64).to_le_bytes());
    hasher.update(text.as_bytes());
}

fn hash_value(hasher: &mut Sha256, value: &Value) {
    match value {
        Value::Null => hasher.update([0]),
        Value::Bool(value) => hasher.update([1, u8::from(*value)]),
        Value::Number(value) => {
            hasher.update([2]);
            hash_text(hasher, &value.to_string());
        }
        Value::String(value) => {
            hasher.update([3]);
            hash_text(hasher, value);
        }
        Value::Array(values) => {
            hasher.update([4]);
            hasher.update((values.len() as u64).to_le_bytes());
            for value in values {
                hash_value(hasher, value);
            }
        }
        Value::Object(values) => {
            hasher.update([5]);
            hasher.update((values.len() as u64).to_le_bytes());
            for (name, value) in values {
                hash_text(hasher, name);
                hash_value(hasher, value);
            }
        }
    }
}

pub(super) fn message_fingerprint(message: &Message) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([match message.role {
        Role::System => 0,
        Role::User => 1,
        Role::Assistant => 2,
        Role::Tool => 3,
    }]);
    hash_text(&mut hasher, &message.content);
    hasher.update((message.tool_calls.len() as u64).to_le_bytes());
    for call in &message.tool_calls {
        hash_text(&mut hasher, &call.id);
        hash_text(&mut hasher, &call.name);
        hash_value(&mut hasher, &call.arguments);
    }
    hasher.update([u8::from(message.tool_call_id.is_some())]);
    if let Some(id) = &message.tool_call_id {
        hash_text(&mut hasher, id);
    }
    hasher.update([u8::from(message.provider_state.is_some())]);
    if let Some(state) = &message.provider_state {
        hash_value(&mut hasher, state);
    }
    hasher.finalize().into()
}

impl SummarizingContext {
    pub(crate) fn message_tokens(&self, message: &Message) -> usize {
        let key = message_fingerprint(message);
        if let Some(count) = self.token_cache.lock().unwrap().counts.get(&key).copied() {
            return count;
        }
        let count = self.uncached_message_tokens(message);
        let mut cache = self.token_cache.lock().unwrap();
        #[cfg(test)]
        {
            cache.computations += 1;
        }
        cache.insert(key, count);
        count
    }
    pub(super) fn uncached_message_tokens(&self, message: &Message) -> usize {
        canonical_message_tokens(message)
    }
    /// Source cadence includes readable reasoning produced by native models,
    /// even when a later serving policy no longer replays that older state.
    pub(crate) fn source_message_tokens(&self, message: &Message) -> usize {
        let mut count = self.message_tokens(message);
        let Some(state) = &message.provider_state else {
            return count;
        };
        if state["adapter"] == "anthropic-messages" {
            return count.saturating_add(self.readable_thinking_tokens(&state["content"], true));
        }
        if state["adapter"] != "chat-completions"
            && !(state.get("adapter").is_none()
                && state.as_object().is_some_and(|fields| {
                    fields.len() == 1 && fields.contains_key("reasoning_content")
                }))
        {
            return count;
        }
        let add_fields = |fields: &Value| {
            let plain = ["reasoning_content", "reasoning"]
                .iter()
                .filter_map(|key| fields[*key].as_str())
                .map(|text| self.tokens(text))
                .sum::<usize>();
            let details = fields["reasoning_details"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|detail| [detail["text"].as_str(), detail["summary"].as_str()])
                .flatten()
                .map(|text| self.tokens(text))
                .sum::<usize>();
            plain.saturating_add(details)
        };
        if let Some(rounds) = state["rounds"]
            .as_array()
            .filter(|rounds| !rounds.is_empty())
        {
            for round in rounds {
                count = count.saturating_add(add_fields(&round["message_fields"]));
            }
        } else {
            count = count.saturating_add(add_fields(state.get("message_fields").unwrap_or(state)));
        }
        count
    }
    pub(super) fn readable_thinking_tokens(&self, blocks: &Value, include_withheld: bool) -> usize {
        blocks
            .as_array()
            .into_iter()
            .flatten()
            .filter(|block| block["type"] == "thinking")
            .filter(|block| include_withheld || block["unsigned_cut"] != true)
            .filter_map(|block| block["thinking"].as_str())
            .map(|text| self.tokens(text))
            .sum()
    }
    pub fn estimated_tokens(&self, request: &ModelRequest) -> usize {
        self.estimated_input(request).0
    }
    pub(super) fn estimated_input(&self, request: &ModelRequest) -> (usize, bool) {
        // The adapter supplies the same messages/schema it uses on the wire,
        // including its replay policy and route/model compatibility checks.
        // Unavailable projections retain the existing canonical estimate.
        let canonical = self.tokens(&request.system_prompt)
            + self.tokens(&json!(request.tools).to_string())
            + 8
            + request
                .messages
                .iter()
                .map(|m| self.message_tokens(m))
                .sum::<usize>();
        match self
            .provider
            .estimate_input_tokens(request, &|input| {
                self.tokens(&input.to_string()).saturating_add(8)
            })
            .ok()
            .flatten()
        {
            Some(estimate) => (canonical.max(estimate), true),
            None => (canonical, false),
        }
    }
}
