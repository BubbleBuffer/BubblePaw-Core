//! The typed blocks a message body is made of: text, and images that name
//! attachments. Marker tags inside a text stay part of its text block.
use crate::Message;
mod content_image;
pub use content_image::{ImageRef, MAX_IMAGE_BYTES, MAX_IMAGES_PER_CALL};
use serde::{Deserialize, Deserializer, Serialize};
use std::{borrow::Cow, sync::Arc};

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ContentBlock {
    Text {
        text: String,
    },
    /// An image by reference. `data` holds the bytes only while a provider
    /// request is built; it is never stored and never serialized.
    Image {
        image: ImageRef,
        #[serde(skip)]
        data: Option<Arc<[u8]>>,
    },
}

/// Hydrated image bytes are never printed: only their length.
impl std::fmt::Debug for ContentBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text { text } => f.debug_struct("Text").field("text", text).finish(),
            Self::Image { image, data } => f
                .debug_struct("Image")
                .field("image", image)
                .field("data_len", &data.as_ref().map(|data| data.len()))
                .finish(),
        }
    }
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
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// The plain-string form: text blocks joined, each image as its placeholder on
/// a line of its own.
fn join_plain(content: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in content {
        match block {
            ContentBlock::Text { text } => out.push_str(text),
            ContentBlock::Image { image, .. } => {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(&content_image::placeholder(image));
            }
        }
    }
    out
}

/// The text of `content`: borrowed when it is one text block, joined otherwise.
/// Images contribute nothing.
pub fn content_text(content: &[ContentBlock]) -> Cow<'_, str> {
    match content {
        [] => Cow::Borrowed(""),
        [ContentBlock::Text { text }] => Cow::Borrowed(text),
        blocks => Cow::Owned(join_text(blocks)),
    }
}

/// The images `content` names, in order.
pub fn content_images(content: &[ContentBlock]) -> impl Iterator<Item = &ImageRef> {
    content.iter().filter_map(|block| match block {
        ContentBlock::Image { image, .. } => Some(image),
        _ => None,
    })
}

/// One block holding `text`, followed by the non-text blocks of `content`.
pub fn with_text(content: &[ContentBlock], text: impl Into<String>) -> Vec<ContentBlock> {
    let mut blocks = text_blocks(text);
    blocks.extend(
        content
            .iter()
            .filter(|block| !matches!(block, ContentBlock::Text { .. }))
            .cloned(),
    );
    blocks
}

impl Message {
    /// The message's text: borrowed when it is one block, the blocks joined
    /// otherwise. Images contribute nothing.
    pub fn text(&self) -> Cow<'_, str> {
        content_text(&self.content)
    }

    /// The message's text, consuming the message.
    pub fn into_text(self) -> String {
        self.text().into_owned()
    }

    /// Replaces the whole body with `text`; images are dropped.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.content = text_blocks(text);
    }

    /// The images the message names, in order.
    /// A message read from a store: a tool message's legacy image envelope
    /// becomes image blocks. Other roles keep their text as written.
    pub(crate) fn upgraded(mut self) -> Self {
        if self.role == crate::Role::Tool {
            self.content = content_image::upgrade_legacy(self.content);
        }
        self
    }
    pub fn images(&self) -> impl Iterator<Item = &ImageRef> {
        content_images(&self.content)
    }
}

thread_local! {
    static PLAIN_CONTENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `f` (a synchronous closure, so no await can run inside) with every `Message.content` serialized as the plain string older
/// readers expect, each image as `[image: <id>, <mime>, <bytes> bytes]`. Size accounting and the formats read by programs outside this
/// repository use it, so their bytes do not depend on the list form.
pub fn plain_content<R>(f: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            PLAIN_CONTENT.with(|flag| flag.set(self.0));
        }
    }
    let _restore = Restore(PLAIN_CONTENT.with(|flag| flag.replace(true)));
    f()
}

/// serde `with` module for `Message.content`.
pub(crate) mod wire {
    use super::*;

    /// Reads the list form, or a plain string (the form older stores and exports
    /// wrote) as one text block. Legacy image envelopes are not read here: only
    /// tool content has them (see `tool` and `Message::upgraded`).
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
            Form::Blocks(mut blocks) => {
                blocks.retain(
                    |block| !matches!(block, ContentBlock::Text { text } if text.is_empty()),
                );
                blocks
            }
        })
    }

    /// Tool content: as `wire`, and a legacy image envelope at the end of the
    /// text becomes image blocks.
    pub(crate) mod tool {
        use super::*;
        pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Vec<ContentBlock>, D::Error> {
            Ok(content_image::upgrade_legacy(super::deserialize(
                deserializer,
            )?))
        }
        pub(crate) use super::serialize;
    }

    pub(crate) fn serialize<S: serde::Serializer>(
        content: &[ContentBlock],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        if PLAIN_CONTENT.with(|flag| flag.get()) {
            serializer.serialize_str(&join_plain(content))
        } else {
            content.serialize(serializer)
        }
    }
}
