use crate::{HistoryCursor, JsonSchema, Message};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a tool declares about itself.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    #[default]
    None,
    Confirm,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    #[serde(default)]
    pub read_only: bool,
    /// Whether calls wait for a person's decision (docs/approvals.md).
    #[serde(default)]
    pub approval: Approval,
}

/// Requested reasoning effort. An unspecified model request uses the provider's
/// low default; it never means reasoning is disabled.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    #[default]
    Low,
    Medium,
    High,
    XHigh,
    Max,
}
impl ReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
}

/// Tier ends of a built request as message counts, for provider cache
/// placement. `cold_end` is one past the last cold message. `previous_len` is
/// the length of this lineage's previous request when this one appends to it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheTiers {
    pub cold_end: Option<usize>,
    pub previous_len: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelRequest {
    pub model: String,
    pub system_prompt: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    /// Inactive declarations retained for signed replay; unavailable for invocation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_tools: Vec<ToolDefinition>,
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Legacy input compatibility only; structured runs use `RunOptions::output_schema`.
    #[serde(default, skip_serializing)]
    pub output_schema: Option<JsonSchema>,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub history: HistoryCursor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_tiers: Option<CacheTiers>,
}
impl ModelRequest {
    pub fn validate_output_schema(&self) -> anyhow::Result<()> {
        if let Some(schema) = &self.output_schema {
            schema.validate()?;
        }
        Ok(())
    }
}
