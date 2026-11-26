//! The wire view of a provider request, for prompt-cache accounting.
use crate::fingerprint;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Request fields that select generation but are not part of the cached prompt.
const GENERATION_FIELDS: &[&str] = &[
    "max_tokens",
    "max_completion_tokens",
    "max_output_tokens",
    "stream",
    "stream_options",
];

/// The final provider request seen as a chain of message boundaries.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WireShape {
    /// Route provider id, for example `opencode-go`; empty when unknown.
    pub provider: String,
    /// Model as forwarded on the wire.
    pub model: String,
    /// `boundaries[0]` hashes every non-message prompt field; each later entry
    /// chains the previous hash with the next wire message (SHA-256, hex).
    pub boundaries: Vec<String>,
    /// Cumulative serialized bytes at each boundary.
    pub boundary_bytes: Vec<u64>,
    /// Bytes of the complete serialized wire body.
    pub wire_bytes: u64,
    pub cache_block_tokens: u64,
}

impl WireShape {
    /// Shape a JSON wire body whose ordered conversation is the array `items`
    /// (`messages` for Chat and Messages, `input` for Responses).
    pub fn from_body(body: &Value, items: &str, cache_block_tokens: u64) -> Self {
        let mut head = without_cache_markers(body);
        let messages = head
            .as_object_mut()
            .and_then(|map| {
                for field in GENERATION_FIELDS {
                    map.remove(*field);
                }
                map.remove(items)
            })
            .and_then(|value| match value {
                Value::Array(values) => Some(values),
                _ => None,
            })
            .unwrap_or_default();
        let head = head.to_string();
        let mut hash = fingerprint(head.as_bytes());
        let mut bytes = head.len() as u64;
        let mut shape = Self {
            boundaries: vec![hash.clone()],
            boundary_bytes: vec![bytes],
            wire_bytes: body.to_string().len() as u64,
            cache_block_tokens: cache_block_tokens.max(1),
            ..Self::default()
        };
        for message in &messages {
            let raw = message.to_string();
            bytes += raw.len() as u64;
            hash = fingerprint(format!("{hash}{raw}").as_bytes());
            shape.boundaries.push(hash.clone());
            shape.boundary_bytes.push(bytes);
        }
        shape
    }
    #[doc(hidden)]
    pub fn group(&self) -> String {
        if self.provider.is_empty() {
            self.model.clone()
        } else {
            format!("{}/{}", self.provider, self.model)
        }
    }
}

/// Cache markers steer where the provider caches; they are not prompt content,
/// so a moved breakpoint is not a prefix break.
fn without_cache_markers(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| *key != "cache_control")
                .map(|(key, value)| (key.clone(), without_cache_markers(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_cache_markers).collect()),
        other => other.clone(),
    }
}

/// Which loop a request belongs to. Only the main loop must be append-only;
/// summary forks and decisions are measured against the shared cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    Main,
    SummaryFork,
    Decider,
}

/// OpenCode Go reuses cached input in 64-token blocks.
pub const DEFAULT_CACHE_BLOCK_TOKENS: u64 = 64;
