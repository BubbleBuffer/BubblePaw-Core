//! Compare Messages wire content while ignoring where `cache_control` sits.
//!
//! Anthropic does not treat breakpoint placement as content, and breakpoints
//! move with every request, so prefix checks compare unmarked copies.
use serde_json::Value;

/// A copy of `value` with every `cache_control` key removed.
pub fn unmarked(value: &Value) -> Value {
    match value {
        Value::Object(map) => map
            .iter()
            .filter(|(key, _)| *key != "cache_control")
            .map(|(key, value)| (key.clone(), unmarked(value)))
            .collect(),
        Value::Array(items) => items.iter().map(unmarked).collect(),
        other => other.clone(),
    }
}

/// Whether the `current` message array starts with every `prior` message,
/// ignoring breakpoint placement. Non-arrays never extend.
pub fn extends(prior: &Value, current: &Value) -> bool {
    let (Some(prior), Some(current)) = (prior.as_array(), current.as_array()) else {
        return false;
    };
    current.len() >= prior.len()
        && prior
            .iter()
            .zip(current)
            .all(|(before, after)| unmarked(before) == unmarked(after))
}
