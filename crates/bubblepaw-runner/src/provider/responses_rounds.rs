//! Responses reply rounds: the length-stop test and the join of a multi-round reply.
use bubblepaw_core::{ModelResponse, Usage};
use serde_json::{Value, json};

/// Whether a Responses object stopped on the output limit (a partial reply).
pub(super) fn is_length_stop(response: &Value) -> bool {
    response["status"] == "incomplete"
        && response["incomplete_details"]["reason"] == "max_output_tokens"
}

/// Joins the rounds of one reply. A single round is returned unchanged. For
/// several rounds, the stored items keep reasoning from every round and carry
/// one assistant message with the joined text, so replay does not repeat text.
pub(super) fn join_rounds(mut rounds: Vec<ModelResponse>) -> ModelResponse {
    if rounds.len() == 1 {
        return rounds.remove(0);
    }
    let mut text = String::new();
    let mut thinking = String::new();
    let mut total = Usage::default();
    let mut any_usage = false;
    let mut items = Vec::new();
    let mut tool_calls = Vec::new();
    let mut last_request_usage = None;
    let mut quota = Vec::new();
    let mut request_usages = Vec::new();
    let (mut model, mut stop_reason) = (None, None);
    for round in rounds {
        request_usages.extend(round.request_usages);
        (model, stop_reason) = (round.model, round.stop_reason);
        text.push_str(&round.text);
        thinking.push_str(&round.thinking);
        if let Some(part) = &round.usage {
            total.add(part);
            any_usage = true;
        }
        if let Some(list) = round
            .provider_state
            .as_ref()
            .and_then(|state| state["items"].as_array())
        {
            // Preserve native ordering, IDs and opaque metadata across ALL
            // rounds. Slim references remove the canonical text duplication.
            items.extend(list.iter().cloned());
        }
        tool_calls = round.tool_calls;
        last_request_usage = round.last_request_usage;
        quota.extend(round.quota);
    }
    ModelResponse {
        text,
        thinking,
        tool_calls,
        usage: any_usage.then_some(total),
        last_request_usage,
        quota,
        model,
        stop_reason,
        request_usages,
        provider_state: Some(json!({"adapter":"responses","items":items})),
        ..Default::default()
    }
}
