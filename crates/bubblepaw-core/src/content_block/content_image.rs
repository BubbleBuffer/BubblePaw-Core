//! Image blocks: the reference a message stores, and the reader for the text
//! envelope older stores wrote in its place.
use crate::ContentBlock;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const MAX_IMAGE_BYTES: usize = 5_000_000;
pub const MAX_IMAGES_PER_CALL: usize = 4;

/// Names an image whose bytes live in the session's attachments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageRef {
    pub id: String,
    pub session_id: String,
    pub mime_type: String,
    pub bytes: usize,
}

impl ImageRef {
    pub fn is_supported_mime(mime: &str) -> bool {
        matches!(
            mime,
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        )
    }
    fn is_well_formed(&self) -> bool {
        !self.session_id.is_empty()
            && self.session_id.len() <= 128
            && Self::is_supported_mime(&self.mime_type)
            && self.bytes <= MAX_IMAGE_BYTES
            && self.id.strip_prefix("image-").is_some_and(|hash| {
                hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
    }
}

impl ContentBlock {
    /// An image block that carries only its reference.
    pub fn image(image: ImageRef) -> Self {
        Self::Image { image, data: None }
    }
    /// The same image block with its bytes filled in for one provider call.
    pub fn image_with_data(image: ImageRef, data: impl Into<Arc<[u8]>>) -> Self {
        Self::Image {
            image,
            data: Some(data.into()),
        }
    }
}

/// The placeholder that stands for an image wherever a plain string is needed.
pub(crate) fn placeholder(image: &ImageRef) -> String {
    format!(
        "[image: {}, {}, {} bytes]",
        image.id, image.mime_type, image.bytes
    )
}

const OPEN: &str = "\n<BubblePawImages>\n";
const CLOSE: &str = "\n</BubblePawImages>";

/// Reads the envelope older stores appended to a message's text. Only a final,
/// line-delimited envelope counts, optionally followed by the tool-budget footer
/// (which stays in the text). Anything else comes back unchanged.
fn split_envelope(content: &str) -> Option<(String, Vec<ImageRef>)> {
    let (body, footer) = content.rsplit_once(CLOSE)?;
    let valid_footer = footer.is_empty()
        || footer
            .strip_prefix("\n\n")
            .and_then(|footer| footer.strip_suffix(" tool calls left"))
            .is_some_and(|count| {
                !count.is_empty() && count.bytes().all(|byte| byte.is_ascii_digit())
            });
    if !valid_footer {
        return None;
    }
    let (text, envelope) = body.rsplit_once(OPEN)?;
    let value: serde_json::Value = serde_json::from_str(envelope).ok()?;
    let images: Vec<ImageRef> = serde_json::from_value(value["images"].clone()).ok()?;
    (!images.is_empty()
        && images.len() <= MAX_IMAGES_PER_CALL
        && images.iter().all(ImageRef::is_well_formed))
    .then(|| (format!("{text}{footer}"), images))
}

/// Turns a legacy envelope at the end of the last text block into image blocks.
pub(crate) fn upgrade_legacy(mut blocks: Vec<ContentBlock>) -> Vec<ContentBlock> {
    let Some(ContentBlock::Text { text }) = blocks.last() else {
        return blocks;
    };
    let Some((text, images)) = split_envelope(text) else {
        return blocks;
    };
    blocks.pop();
    if !text.is_empty() {
        blocks.push(ContentBlock::Text { text });
    }
    blocks.extend(images.into_iter().map(ContentBlock::image));
    blocks
}
