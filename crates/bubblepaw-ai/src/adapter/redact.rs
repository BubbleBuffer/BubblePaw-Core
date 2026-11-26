//! Credential redaction for free-form provider text. Shared by the HTTP error
//! diagnostics and the extension provider.
use regex::Regex;
use std::sync::LazyLock;

pub(super) const REDACTED: &str = "[redacted]";

pub(super) fn redact_text(text: &str, secrets: &[&str]) -> String {
    static HEADERS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?im)^(authorization|proxy-authorization|cookie|set-cookie)\s*:\s*[^\r\n]*")
            .expect("valid header redaction regex")
    });
    static AUTH: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b(bearer|basic)\s+[A-Za-z0-9_./+=:-]+")
            .expect("valid auth redaction regex")
    });
    static FIELD: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)\b(authorization|proxy-authorization|x-api-key|api[-_]?key|token|access[-_]?token|refresh[-_]?token|id[-_]?token|secret|client[-_]?secret|password|cookie|set-cookie)\b(\s*["']?\s*[:=]\s*["']?)[^"'\s,;}\]]+"#)
            .expect("valid secret field redaction regex")
    });
    let mut text = HEADERS.replace_all(text, "$1: [redacted]").into_owned();
    text = AUTH.replace_all(&text, "$1 [redacted]").into_owned();
    text = FIELD.replace_all(&text, "$1$2[redacted]").into_owned();
    // Replace the exact credential before truncation so a cut cannot expose it.
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        text = text.replace(*secret, REDACTED);
    }
    text
}

/// One line of free-form provider text that is safe to show: `redact_text`,
/// then `NAME=value` assignments, `sk-` keys, JWTs and opaque strings of 40 or
/// more characters, whitespace collapsed and cut to `limit` bytes.
pub fn redacted_line(text: &str, secrets: &[&str], limit: usize) -> String {
    static OPAQUE: LazyLock<[(Regex, &str); 4]> = LazyLock::new(|| {
        [
            (
                r#"\b([A-Z][A-Z0-9_]{2,})=("[^"]*"|'[^']*'|\S+)"#,
                "$1=[redacted]",
            ),
            (r"\bsk-[A-Za-z0-9_-]{8,}", REDACTED),
            (r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_.-]+", REDACTED),
            (r"[A-Za-z0-9_+=-]{40,}", REDACTED),
        ]
        .map(|(pattern, with)| (Regex::new(pattern).expect("valid redaction regex"), with))
    });
    let mut text = redact_text(text, secrets);
    for (pattern, with) in OPAQUE.iter() {
        text = pattern.replace_all(&text, *with).into_owned();
    }
    let mut text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&mut text, limit);
    text
}

pub(super) fn truncate(text: &mut String, limit: usize) {
    const SUFFIX: &str = " [truncated]";
    if text.len() > limit {
        let mut end = limit - SUFFIX.len();
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str(SUFFIX);
    }
}
