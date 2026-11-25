use super::{MAX_RESULT, clip};
use serde_json::Value;

/// Room kept for the closing note inside the result limit.
const NOTE_ROOM: usize = 160;

/// Plain text for a direct model call of `read`: the file text for the range,
/// plus one bracketed final line when the text is not the whole file.
pub(super) fn render(value: &Value) -> String {
    let content = value["content"].as_str().unwrap_or_default();
    let start = value["startLine"].as_u64().unwrap_or(1) as usize;
    let total = value["totalLines"].as_u64().unwrap_or(0) as usize;
    if total == 0 {
        return "[empty file]".into();
    }
    if start > total {
        return format!("[no lines: file has {total} lines]");
    }
    let mut text = content;
    let mut cut_line = None;
    // The preview clip can end inside a line; a size cap can end anywhere.
    let mut partial = value["truncated"] == true && !text.ends_with('\n');
    if text.len() + NOTE_ROOM > MAX_RESULT {
        let (head, _) = clip(text, MAX_RESULT - NOTE_ROOM);
        text = head;
        partial = !text.ends_with('\n') && text.len() < content.len();
    }
    if partial {
        match text.rfind('\n') {
            Some(end) => text = &text[..=end],
            None => cut_line = Some(text.len()),
        }
    }
    let lines = text.split_inclusive('\n').count();
    let last = start + lines.max(1) - 1;
    let note = match cut_line {
        Some(bytes) => format!(
            "[line {start} cut at {bytes} bytes; continue with startLine={}]",
            start + 1
        ),
        None if start == 1 && last >= total && !partial && text.len() == content.len() => {
            return text.to_owned();
        }
        None if last < total => {
            format!(
                "[lines {start}-{last} of {total}; continue with startLine={}]",
                last + 1
            )
        }
        None => format!("[lines {start}-{last} of {total}]"),
    };
    let gap = if text.ends_with('\n') { "" } else { "\n" };
    format!("{text}{gap}{note}")
}
