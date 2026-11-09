//! The automatic approval prompt: a template with plain `{name}` placeholders.

/// The prompt sent to the approver when no template is configured.
pub const DEFAULT_AUTO_PROMPT: &str = "\
An agent wants to run a tool call. Decide whether the call stays within what the user asked for \
and is safe to run without a person looking at it first.

User request:
{user_request}

Tool: {tool}
Tool description: {description}
Working directory: {working_directory}
Arguments:
{arguments}

Does this tool call stay within what the user asked for, and is it safe to run without a person \
looking? Answer yes or no.";

/// The values substituted into an approval prompt template.
pub(super) struct PromptFields<'a> {
    pub user_request: &'a str,
    pub tool: &'a str,
    pub description: &'a str,
    pub arguments: &'a str,
    pub working_directory: &'a str,
}

/// The longest `{description}` value, in characters, ellipsis included.
const DESCRIPTION_LIMIT: usize = 300;

/// The value substituted for `{description}`: the first paragraph (up to the
/// first blank line) of the tool description, cut on a character boundary to
/// at most [`DESCRIPTION_LIMIT`] characters with a trailing "…" when cut.
pub(super) fn short_description(description: &str) -> String {
    let mut paragraph = String::new();
    for line in description.trim_start().lines() {
        if line.trim().is_empty() {
            break;
        }
        if !paragraph.is_empty() {
            paragraph.push('\n');
        }
        paragraph.push_str(line);
    }
    let paragraph = paragraph.trim_end();
    if paragraph.chars().count() <= DESCRIPTION_LIMIT {
        return paragraph.to_owned();
    }
    let mut cut: String = paragraph.chars().take(DESCRIPTION_LIMIT - 1).collect();
    cut.push('…');
    cut
}

/// Replaces the known placeholders in one pass, so text inside a substituted
/// value is never substituted again. Unknown `{...}` text is kept as is.
pub(super) fn render(template: &str, fields: &PromptFields<'_>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let value = after.find('}').and_then(|end| {
            let value = match &after[..end] {
                "user_request" => fields.user_request,
                "tool" => fields.tool,
                "description" => fields.description,
                "arguments" => fields.arguments,
                "working_directory" => fields.working_directory,
                _ => return None,
            };
            Some((value, end))
        });
        match value {
            Some((value, end)) => {
                out.push_str(value);
                rest = &after[end + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields() -> PromptFields<'static> {
        PromptFields {
            user_request: "fix {tool}",
            tool: "write",
            description: "Writes a file",
            arguments: "{\"path\":\"a.txt\"}",
            working_directory: "/work",
        }
    }

    #[test]
    fn known_placeholders_are_substituted_once_and_unknown_text_is_kept() {
        let text = render(
            "{user_request}|{tool}|{description}|{arguments}|{working_directory}|{other}|{|}",
            &fields(),
        );
        assert_eq!(
            text,
            "fix {tool}|write|Writes a file|{\"path\":\"a.txt\"}|/work|{other}|{|}"
        );
    }

    #[test]
    fn short_description_keeps_a_short_one_unchanged() {
        assert_eq!(short_description("Writes a file"), "Writes a file");
        assert_eq!(
            short_description("Line one\nline two"),
            "Line one\nline two"
        );
    }

    #[test]
    fn short_description_keeps_only_the_first_paragraph() {
        assert_eq!(
            short_description("Runs a command.\nSecond line.\n\nDetails nobody needs.\n\nMore."),
            "Runs a command.\nSecond line."
        );
        assert_eq!(short_description("A.\r\n\r\nB."), "A.");
    }

    #[test]
    fn short_description_cuts_on_a_character_boundary() {
        let long = "é世".repeat(200);
        let cut = short_description(&long);
        assert_eq!(cut.chars().count(), 300);
        assert!(cut.ends_with('…'));
        assert!(long.starts_with(&cut[..cut.len() - '…'.len_utf8()]));
        let exact = "x".repeat(300);
        assert_eq!(short_description(&exact), exact);
    }

    #[test]
    fn default_prompt_names_every_field() {
        let text = render(DEFAULT_AUTO_PROMPT, &fields());
        for value in [
            "fix {tool}",
            "Tool: write",
            "Writes a file",
            "a.txt",
            "/work",
        ] {
            assert!(text.contains(value), "{value} missing from {text}");
        }
        assert!(!text.contains("{arguments}"));
    }
}
