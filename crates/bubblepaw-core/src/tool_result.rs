//! The outcome of one tool call.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::borrow::Cow;

use crate::{ContentBlock, text_blocks};

/// What a tool returns. `content` is what the model reads, text and image blocks; `details` is
/// JSON for user interfaces and is never sent to a model; `terminate` ends the
/// run once every result of the model reply is recorded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    #[serde(with = "crate::content_block::wire::tool")]
    pub content: Vec<ContentBlock>,
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
        Self::new(text_blocks(content), false, false)
    }
    pub fn error(content: impl Into<String>) -> Self {
        Self::new(text_blocks(content), true, false)
    }
    pub fn pending(content: impl Into<String>) -> Self {
        Self::new(text_blocks(content), false, true)
    }
    fn new(content: Vec<ContentBlock>, is_error: bool, pending: bool) -> Self {
        Self {
            content,
            is_error,
            pending,
            details: None,
            terminate: false,
        }
    }
    /// A result made of the given blocks, for tools that return images.
    pub fn blocks(content: Vec<ContentBlock>) -> Self {
        Self::new(content, false, false)
    }
    /// The text blocks joined; images contribute nothing.
    pub fn body(&self) -> Cow<'_, str> {
        crate::content_text(&self.content)
    }
    /// `content_text` as an owned string.
    pub fn owned_text(&self) -> String {
        self.body().into_owned()
    }
    /// Whether the text contains `needle`.
    pub fn text_has(&self, needle: &str) -> bool {
        self.body().contains(needle)
    }
    /// The images the result names, in order.
    pub fn images(&self) -> impl Iterator<Item = &crate::ImageRef> {
        crate::content_images(&self.content)
    }
    /// Replaces the text with `text`; image blocks stay after it.
    pub fn set_content_text(&mut self, text: impl Into<String>) {
        self.content = crate::with_text(&self.content, text);
    }
    /// Cuts the text to `max` bytes with the truncation marker; images are kept.
    pub(crate) fn bound_text(&mut self, max: usize) {
        let text = self.body();
        if text.len() > max {
            let bounded = crate::stream::bounded(&text, max);
            self.set_content_text(bounded);
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
