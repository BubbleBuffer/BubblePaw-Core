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
