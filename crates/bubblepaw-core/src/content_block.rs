//! The typed blocks a message body is made of. Text is the only kind today;
//! every message holds its text, marker tags included, in one text block.
use crate::Message;
use serde::{Deserialize, Deserializer, Serialize};
use std::borrow::Cow;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ContentBlock {
    Text { text: String },
}

/// The body for a message that holds `text`: one text block, or none when the
/// text is empty (the same body a defaulted message has).
pub fn text_blocks(text: impl Into<String>) -> Vec<ContentBlock> {
    let text = text.into();
    if text.is_empty() {
        Vec::new()
    } else {
        vec![ContentBlock::Text { text }]
    }
}

fn join_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .map(|ContentBlock::Text { text }| text.as_str())
        .collect()
}

impl Message {
    /// The message's text: borrowed when it is one block, the blocks joined otherwise.
    pub fn text(&self) -> Cow<'_, str> {
        match self.content.as_slice() {
            [] => Cow::Borrowed(""),
            [ContentBlock::Text { text }] => Cow::Borrowed(text),
            blocks => Cow::Owned(join_text(blocks)),
        }
    }

    /// The message's text, consuming the message.
    pub fn into_text(self) -> String {
        self.text().into_owned()
    }

    /// Replaces the whole body with `text`.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.content = text_blocks(text);
    }
}

thread_local! {
    static PLAIN_CONTENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `f` with every `Message.content` serialized as the plain string older
/// readers expect. Size accounting and the formats read by programs outside this
/// repository use it, so their bytes do not depend on the list form.
pub fn plain_content<R>(f: impl FnOnce() -> R) -> R {
    let before = PLAIN_CONTENT.with(|flag| flag.replace(true));
    let result = f();
    PLAIN_CONTENT.with(|flag| flag.set(before));
    result
}

/// serde `with` module for `Message.content`.
pub(crate) mod wire {
    use super::*;

    /// Reads the list form, or a plain string (the form older stores and exports
    /// wrote) as one text block.
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<ContentBlock>, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Form {
            Text(String),
            Blocks(Vec<ContentBlock>),
        }
        Ok(match Form::deserialize(deserializer)? {
            Form::Text(text) => text_blocks(text),
            Form::Blocks(blocks) => blocks,
        })
    }

    pub(crate) fn serialize<S: serde::Serializer>(
        content: &[ContentBlock],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        if PLAIN_CONTENT.with(|flag| flag.get()) {
            serializer.serialize_str(&join_text(content))
        } else {
            content.serialize(serializer)
        }
    }
}
