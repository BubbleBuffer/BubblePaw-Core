use crate::{ContentBlock, IncompleteReason, ToolCall, content_block};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    #[default]
    User,
    Assistant,
    Tool,
}

/// Message provenance, independent of its attachment envelope.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageOrigin {
    #[default]
    Typed,
    ChildTask,
    Generated,
    /// Replay-only neutral marker; the archive contains no recorded tool outcome.
    ArchiveUnknownOutcome,
}

/// A user message is typed unless its origin says otherwise. A missing origin
/// (only in-memory requests; every persisted writer sets one) counts as typed.
pub fn is_typed_user(message: &Message) -> bool {
    message.role == Role::User
        && match message.origin {
            Some(MessageOrigin::Typed) => true,
            Some(
                MessageOrigin::ChildTask
                | MessageOrigin::Generated
                | MessageOrigin::ArchiveUnknownOutcome,
            ) => false,
            None => true,
        }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub id: String,
    pub session_id: String,
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<MessageOrigin>,
    #[serde(with = "content_block::wire")]
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_state: Option<Value>,
    #[serde(default)]
    pub is_error: bool,
    /// Data for user interfaces; never sent to a model (docs/thinking-tool-loop.md).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// Set on an assistant reply whose request failed or was cancelled after
    /// text had streamed (docs/thinking-tool-loop.md).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incomplete: Option<IncompleteReason>,
    pub created_at: String,
}
impl Message {
    /// The model-facing form of a stored message: `details` is for interfaces only.
    #[doc(hidden)]
    pub fn without_details(self) -> Self {
        Self {
            details: None,
            ..self
        }
    }
}
