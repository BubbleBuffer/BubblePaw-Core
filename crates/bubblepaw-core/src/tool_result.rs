//! The outcome of one tool call.
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a tool returns. `content` is the text the model reads; `details` is
/// JSON for user interfaces and is never sent to a model; `terminate` ends the
/// run once every result of the model reply is recorded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: String,
    #[serde(default)]
    pub is_error: bool,
    /// The effect has started but is still owned by the host. Its implementation
    /// settles the boundary when the asynchronous operation actually terminates.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub terminate: bool,
}
impl ToolResult {
    pub fn text(content: impl Into<String>) -> Self {
        Self::new(content.into(), false, false)
    }
    pub fn error(content: impl Into<String>) -> Self {
        Self::new(content.into(), true, false)
    }
    pub fn pending(content: impl Into<String>) -> Self {
        Self::new(content.into(), false, true)
    }
    fn new(content: String, is_error: bool, pending: bool) -> Self {
        Self {
            content,
            is_error,
            pending,
            details: None,
            terminate: false,
        }
    }
    /// Attach data for user interfaces; it never reaches a model.
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
    /// End the run after every result of this model reply is recorded.
    pub fn with_terminate(mut self) -> Self {
        self.terminate = true;
        self
    }
}

impl crate::Message {
    /// The model-facing form of a stored message: `details` is for interfaces only.
    pub(crate) fn without_details(self) -> Self {
        Self {
            details: None,
            ..self
        }
    }
}
