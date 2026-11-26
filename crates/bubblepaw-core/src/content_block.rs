//! The typed blocks a message body is made of: text, images that name
//! attachments, and uploaded files. Marker tags inside a text stay part of its
//! text block.
use crate::Message;
mod content_file;
mod content_image;
mod message_input;
pub use content_file::{InputFileRef, file_envelope, show_files_as_text};
pub use content_image::{ImageRef, MAX_IMAGE_BYTES, MAX_IMAGES_PER_CALL};
pub use message_input::MessageInput;
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
    /// An uploaded file by reference, with the text snapshot the model reads.
    File {
        file: InputFileRef,
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
            Self::File { file } => f.debug_struct("File").field("file", file).finish(),
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

/// The plain-string form: text blocks joined, each image and file as its
/// placeholder on a line of its own.
fn join_plain(content: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in content {
        let placeholder = match block {
            ContentBlock::Text { text } => {
                out.push_str(text);
                continue;
            }
            ContentBlock::Image { image, .. } => content_image::placeholder(image),
            ContentBlock::File { file } => content_file::placeholder(file),
        };
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&placeholder);
    }
    out
}

/// The written text of `content`, without its files: borrowed when it is one
/// text block, joined otherwise. Images contribute nothing.
pub fn content_body_text(content: &[ContentBlock]) -> Cow<'_, str> {
    match content {
        [] => Cow::Borrowed(""),
        [ContentBlock::Text { text }] => Cow::Borrowed(text),
        blocks => Cow::Owned(join_text(blocks)),
    }
}

/// The text the model reads for `content`: the written text, then the envelope
/// that shows it the files. This is the string a message whose files were
/// appended as text had, so the request a provider builds does not depend on
/// the block form.
pub fn content_text(content: &[ContentBlock]) -> Cow<'_, str> {
    let body = content_body_text(content);
    if content_files(content).next().is_none() {
        return body;
    }
    Cow::Owned(body.into_owned() + &content_file::render(content_files(content)))
}

/// The images `content` names, in order.
pub fn content_images(content: &[ContentBlock]) -> impl Iterator<Item = &ImageRef> {
    content.iter().filter_map(|block| match block {
        ContentBlock::Image { image, .. } => Some(image),
        _ => None,
    })
}

/// The files `content` names, in order.
pub fn content_files(content: &[ContentBlock]) -> impl Iterator<Item = &InputFileRef> {
    content.iter().filter_map(|block| match block {
        ContentBlock::File { file } => Some(file),
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
    /// The text the model reads: the written text, then the envelope that shows
    /// it the message's files. Images contribute nothing.
    pub fn text(&self) -> Cow<'_, str> {
        content_text(&self.content)
    }

    /// Only the written text, without the files.
    pub fn body_text(&self) -> Cow<'_, str> {
        content_body_text(&self.content)
    }

    /// The message's text, consuming the message.
    pub fn into_text(self) -> String {
        self.text().into_owned()
    }

    /// Replaces the whole body with `text`; images and files are dropped.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.content = text_blocks(text);
    }

    /// A message read from a store: a tool message's legacy image envelope
    /// becomes image blocks, and a user message's legacy file envelope becomes
    /// file blocks. Other roles keep their text as written.
    pub(crate) fn upgraded(mut self) -> Self {
        match self.role {
            crate::Role::Tool => self.content = content_image::upgrade_legacy(self.content),
            crate::Role::User => self.content = upgrade_user_content(self.content),
            _ => {}
        }
        self
    }

    /// The images the message names, in order.
    pub fn images(&self) -> impl Iterator<Item = &ImageRef> {
        content_images(&self.content)
    }

    /// The files the message names, in order.
    pub fn files(&self) -> impl Iterator<Item = &InputFileRef> {
        content_files(&self.content)
    }
}

/// A user message's blocks as stored rows and queued inputs read them: a legacy
/// file envelope at the end of the text becomes file blocks.
pub(crate) fn upgrade_user_content(content: Vec<ContentBlock>) -> Vec<ContentBlock> {
    content_file::upgrade_legacy(content)
}

/// `content` when it holds more than text, nothing otherwise: the form events and
/// queued inputs carry beside their text.
pub(crate) fn extra_content(content: &[ContentBlock]) -> Vec<ContentBlock> {
    if content
        .iter()
        .any(|block| !matches!(block, ContentBlock::Text { .. }))
    {
        content.to_vec()
    } else {
        Vec::new()
    }
}

thread_local! {
    static PLAIN_CONTENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `f` (a synchronous closure, so no await can run inside) with every `Message.content` serialized as the plain string older
/// readers expect, each image as `[image: <id>, <mime>, <bytes> bytes]` and each file as `[file: <id>, <name>, <bytes> bytes]`. Size
/// accounting and the formats read by programs outside this repository use it, so their bytes do not depend on the list form.
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
    /// wrote) as one text block. Legacy envelopes are not read here: only tool
    /// content and user messages have them (see `tool` and `Message::upgraded`).
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
