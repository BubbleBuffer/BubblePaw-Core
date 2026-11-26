//! A model reply and the facts about it that the loop and core read as typed fields.
//!
//! An adapter fills the typed fields from what its provider reports; a provider
//! that reports nothing leaves them `None` or empty. `provider_state` stays the
//! adapter's own replay blob. Each fact has one accessor that reads the typed
//! field first and falls back to the blob for replies that carry only the blob.
use crate::{LastRequestUsage, QuotaReading, RequestLogRow, ToolCall, Usage};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Why a model stopped generating, as far as the provider says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StopReason {
    /// The model finished its turn.
    EndTurn,
    /// The model stopped to call tools.
    ToolUse,
    /// The reply was cut at the output token limit.
    MaxOutputTokens,
    /// The provider filtered or refused the reply.
    ContentFilter,
    /// Any other provider reason, kept as reported.
    Other(String),
}

/// Which configured route served a reply and the routes that failed before it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServedRoute {
    /// The qualified `provider/model` route that answered.
    pub selected: String,
    /// Routes tried and failed before it, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback_from: Vec<String>,
    /// Classified failure reasons, one per failed route.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelResponse {
    pub text: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub thinking: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quota: Vec<QuotaReading>,
    /// The final physical request, apart from cumulative continuation usage; absent for one logical response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_request_usage: Option<LastRequestUsage>,
    /// The model id the provider reported for the reply, when it reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Why the final request stopped, when the provider reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    /// Usage of each physical request an adapter made for the reply (length
    /// continuations, reply rounds), in order; empty when it reports only the
    /// total. `usage` also counts the usage of routes that failed before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub request_usages: Vec<Usage>,
    /// The route that served the reply; set by the registry when fallbacks are configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<ServedRoute>,
    /// Wire positions of the cache marks sent (`system.0`, `messages.4.1`); `None` when an adapter places none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_breakpoints: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_state: Option<Value>,
}
impl ModelResponse {
    /// Deserialize a complete JSON reply into a host-owned type (serde enforces the contract, not JSON Schema).
    pub fn parse_json<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        ensure!(
            self.tool_calls.is_empty(),
            "cannot parse a tool-call response as a JSON reply"
        );
        serde_json::from_str(&self.text)
            .context("model reply is not valid JSON for the requested type")
    }
    /// Serialized size of the reply as the storage limit measures it: without the
    /// typed reply facts, each of which adds `,"name":value` when present.
    pub(crate) fn storage_bytes(&self) -> serde_json::Result<usize> {
        fn field<T: Serialize>(name: &str, value: &Option<T>) -> serde_json::Result<usize> {
            match value {
                Some(value) => Ok(name.len() + 4 + serde_json::to_vec(value)?.len()),
                None => Ok(0),
            }
        }
        let list = Some(&self.request_usages).filter(|usages| !usages.is_empty());
        let extra = field("model", &self.model)?
            + field("stop_reason", &self.stop_reason)?
            + field("request_usages", &list)?
            + field("route", &self.route)?
            + field("cache_breakpoints", &self.cache_breakpoints)?;
        Ok(serde_json::to_vec(self)?.len() - extra)
    }
    /// The qualified route that served the reply.
    pub fn selected_route(&self) -> Option<&str> {
        match &self.route {
            Some(route) => Some(route.selected.as_str()),
            None => self
                .legacy_state_field("_bubblepaw_selected_model")?
                .as_str(),
        }
    }
    /// The routes that failed before the serving one, with their classified reasons.
    pub fn fallbacks(&self) -> (Vec<String>, Vec<String>) {
        if let Some(route) = &self.route {
            return (route.fallback_from.clone(), route.fallback_reasons.clone());
        }
        let list = |key| -> Vec<String> {
            serde_json::from_value(self.legacy_state_field(key).cloned().unwrap_or_default())
                .unwrap_or_default()
        };
        (
            list("_bubblepaw_fallback_from"),
            list("_bubblepaw_fallback_reasons"),
        )
    }
    /// Whether the reply took more than one physical request.
    pub fn multiple_requests(&self) -> bool {
        if !self.request_usages.is_empty() {
            return self.request_usages.len() > 1;
        }
        // Legacy: replies without a typed list count the blob's rounds.
        self.provider_state
            .as_ref()
            .is_some_and(|state| state["rounds"].as_array().is_some_and(|r| r.len() > 1))
    }
    /// The cache marks the adapter reported for the request it sent.
    pub fn reported_cache_breakpoints(&self) -> Option<Vec<String>> {
        if self.cache_breakpoints.is_some() {
            return self.cache_breakpoints.clone();
        }
        let marks = self.legacy_state_field(RequestLogRow::BREAKPOINTS_KEY)?;
        serde_json::from_value(marks.clone()).ok()
    }
    /// Legacy: a fact that older replies and extension providers carry only as a key of `provider_state`.
    fn legacy_state_field(&self, key: &str) -> Option<&Value> {
        self.provider_state.as_ref()?.get(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A reply with typed facts and one with only the old blob keys answer alike.
    #[test]
    fn typed_and_blob_only_replies_give_the_same_facts() {
        let typed = ModelResponse {
            route: Some(ServedRoute {
                selected: "b/m".into(),
                fallback_from: vec!["a/m".into()],
                fallback_reasons: vec!["rate_limited".into()],
            }),
            request_usages: vec![Usage::default(), Usage::default()],
            cache_breakpoints: Some(vec!["system.0".into()]),
            ..Default::default()
        };
        let legacy = ModelResponse {
            provider_state: Some(json!({
                "_bubblepaw_selected_model": "b/m",
                "_bubblepaw_fallback_from": ["a/m"],
                "_bubblepaw_fallback_reasons": ["rate_limited"],
                "_bubblepaw_cache_breakpoints": ["system.0"],
                "rounds": [{}, {}],
            })),
            ..Default::default()
        };
        for reply in [&typed, &legacy] {
            assert_eq!(reply.selected_route(), Some("b/m"));
            assert_eq!(
                reply.fallbacks(),
                (vec!["a/m".to_owned()], vec!["rate_limited".to_owned()])
            );
            assert!(reply.multiple_requests());
            assert_eq!(
                reply.reported_cache_breakpoints(),
                Some(vec!["system.0".to_owned()])
            );
        }
        // The storage limit measures the reply without the typed facts.
        let plain = ModelResponse {
            text: "x".into(),
            ..Default::default()
        };
        let typed = ModelResponse {
            stop_reason: Some(StopReason::EndTurn),
            model: Some("m".into()),
            ..plain.clone()
        };
        assert_eq!(
            plain.storage_bytes().unwrap(),
            typed.storage_bytes().unwrap()
        );
        assert!(
            serde_json::to_vec(&plain).unwrap().len() < serde_json::to_vec(&typed).unwrap().len()
        );
        let bare = ModelResponse::default();
        assert!(bare.selected_route().is_none() && !bare.multiple_requests());
    }
}
