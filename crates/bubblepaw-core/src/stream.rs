pub(crate) fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.into();
    }
    const MARKER: &str = "\n[tool result truncated]";
    let room = max.saturating_sub(MARKER.len());
    let mut end = room.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if max < MARKER.len() {
        return MARKER[..max].into();
    }
    format!("{}{}", &text[..end], MARKER)
}

/// Emits complete paragraphs, keeping fenced code in one block. Provider deltas
/// never become public events or stored conversation messages.
#[derive(Default)]
pub(crate) struct Paragraphs {
    line: String,
    paragraph: String,
    fence: Option<(char, usize)>,
    received: bool,
}
impl Paragraphs {
    pub fn push(&mut self, text: &str) -> Vec<String> {
        self.received |= !text.is_empty();
        let mut blocks = Vec::new();
        for ch in text.chars() {
            self.line.push(ch);
            if ch == '\n' {
                self.complete_line(&mut blocks);
            }
        }
        blocks
    }
    fn complete_line(&mut self, blocks: &mut Vec<String>) {
        let trimmed = self.line.trim();
        let candidate = trimmed.chars().next().filter(|c| *c == '`' || *c == '~');
        let count = candidate
            .map(|c| trimmed.chars().take_while(|x| *x == c).count())
            .unwrap_or(0);
        if let Some((character, width)) = self.fence {
            self.paragraph.push_str(&self.line);
            if candidate == Some(character)
                && count >= width
                && trimmed.chars().skip(count).all(char::is_whitespace)
            {
                self.fence = None;
                self.emit(blocks);
            }
        } else if count >= 3 {
            self.emit(blocks);
            self.fence = candidate.map(|c| (c, count));
            self.paragraph.push_str(&self.line);
        } else if trimmed.is_empty() {
            self.emit(blocks);
        } else {
            self.paragraph.push_str(&self.line);
        }
        self.line.clear();
    }
    fn emit(&mut self, blocks: &mut Vec<String>) {
        if !self.paragraph.trim().is_empty() {
            blocks.push(self.paragraph.trim_end_matches(['\r', '\n']).to_owned());
        }
        self.paragraph.clear();
    }
    pub fn finish(&mut self) -> Vec<String> {
        let mut blocks = Vec::new();
        if !self.line.is_empty() {
            self.complete_line(&mut blocks);
        }
        self.emit(&mut blocks);
        blocks
    }
    pub fn had_text(&self) -> bool {
        self.received
    }
    pub fn push_bounded(&mut self, text: &str, max: usize) -> Vec<String> {
        let mut blocks = Vec::new();
        for ch in text.chars() {
            blocks.extend(self.push(&ch.to_string()));
            if self.line.len() + self.paragraph.len() >= max.saturating_sub(3).max(1) {
                self.paragraph.push_str(&self.line);
                self.line.clear();
                self.emit(&mut blocks);
            }
        }
        blocks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn emits_paragraphs_and_never_splits_fenced_code() {
        let mut p = Paragraphs::default();
        assert!(p.push("First para").is_empty());
        assert_eq!(
            p.push("graph.\n\n```rust\nfn main() {\n\n"),
            vec!["First paragraph."]
        );
        assert!(p.push("    println!(\"ok\");\n}\n``").is_empty());
        assert_eq!(
            p.push("`\n\nLast"),
            vec!["```rust\nfn main() {\n\n    println!(\"ok\");\n}\n```"]
        );
        assert_eq!(p.finish(), vec!["Last"]);
    }
}
