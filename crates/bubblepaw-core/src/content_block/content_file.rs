//! File blocks: the reference a message stores for an uploaded file, how the
//! model is shown it, and the reader for the text envelope older stores wrote.
use crate::ContentBlock;
use serde::{Deserialize, Serialize};

/// An uploaded file by reference. `inline` is a snapshot of the file's text taken when the
/// message was accepted; it is what the model reads, and nothing re-reads the file later.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputFileRef {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub path: String,
    pub url: String,
    pub text: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted: Option<bool>,
}

impl ContentBlock {
    /// A file block holding the file's reference.
    pub fn file(file: InputFileRef) -> Self {
        Self::File { file }
    }
}

/// The placeholder that stands for a file wherever a plain string is needed.
pub(crate) fn placeholder(file: &InputFileRef) -> String {
    format!("[file: {}, {}, {} bytes]", file.id, file.name, file.size)
}

const OPEN: &str = "\n\n<BubblePawAttachments>\n";
const CLOSE: &str = "\n</BubblePawAttachments>";

/// The text the model reads for `files`: a JSON envelope on lines of its own, appended to the
/// message's text.
pub(crate) fn render<'a>(files: impl IntoIterator<Item = &'a InputFileRef>) -> String {
    let files: Vec<&InputFileRef> = files.into_iter().collect();
    format!("{OPEN}{}{CLOSE}", serde_json::json!({ "files": files }))
}

/// Reads the envelope older stores appended to a user message's text. Only a final envelope that
/// this crate would render byte for byte counts, so the model reads the same text either way.
/// Anything else comes back unchanged.
fn split_envelope(content: &str) -> Option<(&str, Vec<InputFileRef>)> {
    let start = content.rfind(OPEN)?;
    let tail = &content[start..];
    let body = tail.strip_prefix(OPEN)?.strip_suffix(CLOSE)?;
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let files: Vec<InputFileRef> = serde_json::from_value(value.get("files")?.clone()).ok()?;
    (!files.is_empty() && render(&files) == tail).then(|| (&content[..start], files))
}

/// Turns a legacy envelope at the end of the last text block into file blocks.
pub(crate) fn upgrade_legacy(mut blocks: Vec<ContentBlock>) -> Vec<ContentBlock> {
    let Some(ContentBlock::Text { text }) = blocks.last() else {
        return blocks;
    };
    let Some((text, files)) = split_envelope(text) else {
        return blocks;
    };
    let text = text.to_owned();
    blocks.pop();
    if !text.is_empty() {
        blocks.push(ContentBlock::Text { text });
    }
    blocks.extend(files.into_iter().map(ContentBlock::file));
    blocks
}

/// The envelope text for `files`, as the model reads it after a message's written text.
pub fn file_envelope<'a>(files: impl IntoIterator<Item = &'a InputFileRef>) -> String {
    render(files)
}

/// Replaces the file blocks of each message with the text the model reads for them, so a
/// consumer that serializes messages as plain strings (which show a file as a placeholder)
/// still hands its model the file's text.
pub fn show_files_as_text(messages: &mut [crate::Message]) {
    for message in messages
        .iter_mut()
        .filter(|message| message.files().next().is_some())
    {
        let mut content = crate::text_blocks(message.text().into_owned());
        content.extend(
            message
                .content
                .drain(..)
                .filter(|block| matches!(block, ContentBlock::Image { .. })),
        );
        message.content = content;
    }
}
