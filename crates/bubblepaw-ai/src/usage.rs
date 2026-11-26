use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Cached tokens are subsets of input tokens, not additional input totals.
    #[serde(default, deserialize_with = "deserialize_cache_read_tokens")]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_creation_tokens: u64,
    /// Reasoning tokens, a subset of output tokens, when the provider reports
    /// them. `None` means not reported; it is omitted when serialized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}
impl Usage {
    /// Accumulate another request's usage into run totals.
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .saturating_add(other.cache_creation_tokens);
        self.reasoning_tokens = match (self.reasoning_tokens, other.reasoning_tokens) {
            (Some(a), Some(b)) => Some(a.saturating_add(b)),
            (a, b) => a.or(b),
        };
    }
}

/// The final physical request's usage, apart from cumulative continuation usage.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LastRequestUsage {
    /// Whole-prompt input of the final physical request. None means that request omitted it.
    pub input_tokens: Option<u64>,
    /// Output of that same physical request, separate from cumulative continuations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
}

// Earlier archives used Option<u64>, serializing explicit unknown values as null.
fn deserialize_cache_read_tokens<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<u64>::deserialize(deserializer)?.unwrap_or(0))
}
