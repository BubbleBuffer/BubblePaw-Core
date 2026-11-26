//! What a caller submits as a user message.
use crate::{ContentBlock, text_blocks};
use std::borrow::Cow;

/// The body of a user message: text, or blocks. Anything that was a string still converts.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct MessageInput(Vec<ContentBlock>);

impl MessageInput {
    pub fn blocks(&self) -> &[ContentBlock] {
        &self.0
    }
    pub fn into_blocks(self) -> Vec<ContentBlock> {
        self.0
    }
    /// The text the model reads for this input (files included, as for a stored message).
    pub fn text(&self) -> Cow<'_, str> {
        super::content_text(&self.0)
    }
}
impl From<Vec<ContentBlock>> for MessageInput {
    fn from(blocks: Vec<ContentBlock>) -> Self {
        Self(blocks)
    }
}
impl From<String> for MessageInput {
    fn from(text: String) -> Self {
        Self(text_blocks(text))
    }
}
impl From<&str> for MessageInput {
    fn from(text: &str) -> Self {
        Self(text_blocks(text))
    }
}
impl From<&String> for MessageInput {
    fn from(text: &String) -> Self {
        Self(text_blocks(text.as_str()))
    }
}
impl From<Cow<'_, str>> for MessageInput {
    fn from(text: Cow<'_, str>) -> Self {
        Self(text_blocks(text.into_owned()))
    }
}
