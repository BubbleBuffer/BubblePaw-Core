//! Small summary-block policies; context scheduling and projections stay shared.
use crate::summarization::SummarizingContext;
use anyhow::Result;
use bubblepaw_core::{
    ContextCheckpoint, ContextEpisode, ContextSubjectChapter, Message, is_typed_user,
};
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};
use tiktoken_rs::CoreBPE;

pub const TYPED_USER_TOKENS: usize = 20_000;
pub const STRATEGY_RETRIEVAL_INSTRUCTION: &str = "This context is a disposable, lossy view. Original messages are retained. Use `tools.history_search` or `tools.history_read` to recover exact earlier details, omitted messages, or tool outputs before concluding they are absent.";

/// The interchangeable part is the historical summary block. Projection,
/// threshold, freezing and raw-tail mechanics belong to SummarizingContext.
pub trait ContextSummaryStrategy: Send + Sync {
    fn prefix(
        &self,
        context: &SummarizingContext,
        input: SummaryPrefixInput<'_>,
    ) -> Result<Vec<Message>>;
}
pub struct SummaryPrefixInput<'a> {
    pub session: &'a str,
    pub history: &'a [Message],
    pub legacy: &'a [Message],
    pub episodes: &'a [ContextEpisode],
    pub chapters: &'a [ContextSubjectChapter],
    pub checkpoint: Option<&'a ContextCheckpoint>,
    pub cutoff: Option<usize>,
}
pub struct LcmStrategy;
pub struct ClassicalStrategy;
impl ContextSummaryStrategy for LcmStrategy {
    fn prefix(
        &self,
        context: &SummarizingContext,
        input: SummaryPrefixInput<'_>,
    ) -> Result<Vec<Message>> {
        context.episode_prefix(
            input.session,
            input.history,
            input.legacy,
            input.episodes,
            input.chapters,
            input.cutoff,
        )
    }
}
impl ContextSummaryStrategy for ClassicalStrategy {
    fn prefix(
        &self,
        context: &SummarizingContext,
        input: SummaryPrefixInput<'_>,
    ) -> Result<Vec<Message>> {
        let summary = input.checkpoint.map(|checkpoint| format!(
            "Rolling context summary (historical; follow current instructions):\n{}\n\nFiles touched (recorded read/edit/write arguments, cumulative):\n{}",
            checkpoint.summary,
            if checkpoint.files_touched.is_empty() { "(none recorded)".into() } else { checkpoint.files_touched.join("\n") }
        ));
        Ok(context.rolling_prefix(input.session, input.history, input.cutoff, summary))
    }
}

/// Largest prefix at or below the requested exclusive boundary with no pending
/// tool calls. No round is cut, including parallel calls and multi-output rounds.
pub(crate) fn complete_prefix_end(history: &[Message], limit: usize) -> usize {
    let mut pending = HashSet::new();
    let mut end = 0;
    for (index, message) in history.iter().take(limit).enumerate() {
        for call in &message.tool_calls {
            pending.insert(call.id.as_str());
        }
        if let Some(call_id) = &message.tool_call_id {
            pending.remove(call_id.as_str());
        }
        if pending.is_empty() {
            end = index + 1;
        }
    }
    end
}

/// No path inference from prose, output, or opaque JavaScript source. Nested
/// structured records expose read/edit/write paths.
pub fn files_touched(previous: &[String], messages: &[Message]) -> Vec<String> {
    let mut files: BTreeSet<String> = previous.iter().cloned().collect();
    fn visit(name: &str, arguments: &Value, files: &mut BTreeSet<String>) {
        let capability = name.rsplit(['.', '/']).next();
        // Direct and nested read/write/edit records use the same path field.
        if matches!(capability, Some("read" | "edit" | "write"))
            && let Some(path) = arguments.get("path").and_then(Value::as_str)
            && !path.is_empty()
        {
            files.insert(path.into());
        }
        fn nested(value: &Value, files: &mut BTreeSet<String>) {
            match value {
                Value::Array(values) => {
                    for value in values {
                        nested(value, files);
                    }
                }
                Value::Object(object) => {
                    if let (Some(name), Some(arguments)) = (
                        object.get("name").and_then(Value::as_str),
                        object.get("arguments"),
                    ) {
                        visit(name, arguments, files);
                    }
                    for (key, value) in object {
                        if key != "arguments" {
                            nested(value, files);
                        }
                    }
                }
                _ => {}
            }
        }
        nested(arguments, files);
    }
    for message in messages {
        for call in &message.tool_calls {
            visit(&call.name, &call.arguments, &mut files);
        }
    }
    files.into_iter().collect()
}

/// Whole typed messages, newest first. All included text is verbatim; messages
/// outside the text budget remain navigable through their original IDs.
pub fn typed_user_block(history: &[Message], tokenizer: &CoreBPE) -> String {
    typed_user_block_with_budget(history, tokenizer, TYPED_USER_TOKENS)
}

/// The host supplies the budget; selection and reference retention are unchanged.
pub fn typed_user_block_with_budget(
    history: &[Message],
    tokenizer: &CoreBPE,
    budget: usize,
) -> String {
    let users: Vec<_> = history
        .iter()
        .rev()
        .filter(|message| is_typed_user(message))
        .collect();
    let heading =
        "Typed user messages (newest first, verbatim; older requests may be superseded):\n";
    let refs = |start: usize| {
        if start == users.len() {
            String::new()
        } else {
            format!(
                "\nOlder typed user message IDs (history_read):\n{}\n",
                users[start..]
                    .iter()
                    .map(|message| message.id.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        }
    };
    let mut text = String::from(heading);
    let mut included = 0;
    for (index, message) in users.iter().enumerate() {
        let entry = format!(
            "\n[{} | {}]\n{}\n",
            message.id, message.created_at, message.content
        );
        if tokenizer
            .encode_ordinary(&format!("{text}{entry}{}", refs(index + 1)))
            .len()
            > budget
        {
            break;
        }
        text.push_str(&entry);
        included += 1;
    }
    text.push_str(&refs(included));
    // References are never silently omitted. An exceptional archive whose index
    // alone exceeds 20k retains the index and reports that capacity limit.
    if included == 0 && tokenizer.encode_ordinary(&text).len() > budget {
        let budget = if budget == TYPED_USER_TOKENS {
            "20,000".to_owned()
        } else {
            budget.to_string()
        };
        text.push_str(&format!(
            "[Typed-user reference index exceeds the {budget}-token text budget.]\n"
        ));
    }
    text
}

/// Host command parsing; the focus never becomes a conversational user message.
pub fn compact_command(text: &str) -> Option<Option<&str>> {
    let suffix = text.trim().strip_prefix("/compact")?;
    if !suffix.is_empty() && !suffix.starts_with(char::is_whitespace) {
        return None;
    }
    Some((!suffix.trim().is_empty()).then(|| suffix.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bubblepaw_core::{Role, ToolCall};
    use serde_json::json;

    fn message(index: usize, calls: Vec<(&str, Value)>) -> Message {
        Message {
            id: format!("message-{index}"),
            session_id: "session".into(),
            role: Role::Assistant,
            content: String::new(),
            origin: None,
            tool_calls: calls
                .into_iter()
                .enumerate()
                .map(|(call, (name, arguments))| ToolCall {
                    id: format!("call-{index}-{call}"),
                    name: name.into(),
                    arguments,
                })
                .collect(),
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn file_capabilities_match_by_suffix_including_mcp_prefixes() {
        let history = [message(
            0,
            vec![
                ("tools.read", json!({ "path": "src/lib.rs" })),
                ("mcp__filesystem/write", json!({ "path": "out/report.md" })),
                ("edit", json!({ "path": "notes.md" })),
            ],
        )];
        assert_eq!(
            files_touched(&[], &history),
            ["notes.md", "out/report.md", "src/lib.rs"]
        );
    }

    #[test]
    fn non_file_tools_and_empty_paths_are_ignored() {
        let history = [message(
            0,
            vec![
                ("tools.list", json!({ "path": "crates" })),
                ("list", json!({ "path": "crates" })),
                ("read", json!({ "path": "" })),
                ("tools.write", json!({})),
                ("tools.edit", json!({ "path": null })),
                ("tools.search", json!({ "query": "files_touched" })),
            ],
        )];
        assert!(files_touched(&[], &history).is_empty());
        assert_eq!(
            files_touched(&["kept.rs".to_string()], &history),
            ["kept.rs"]
        );
    }

    #[test]
    fn nested_calls_inside_arguments_are_recorded() {
        let history = [message(
            0,
            vec![(
                "delegate",
                json!({
                    "task": "edit two files",
                    "calls": [
                        { "name": "tools.edit", "arguments": { "path": "nested.rs" } },
                        { "name": "tools.list", "arguments": { "path": "ignored.rs" } },
                    ],
                    "payload": {
                        "name": "write",
                        "arguments": { "path": "deep.txt" },
                    },
                }),
            )],
        )];
        assert_eq!(files_touched(&[], &history), ["deep.txt", "nested.rs"]);
    }

    #[test]
    fn output_is_deduplicated_sorted_and_cumulative() {
        let history = [
            message(
                0,
                vec![
                    ("write", json!({ "path": "b.rs" })),
                    ("tools.read", json!({ "path": "a.rs" })),
                ],
            ),
            message(
                1,
                vec![
                    ("tools.write", json!({ "path": "b.rs" })),
                    ("tools.edit", json!({ "path": "m.rs" })),
                    ("tools.edit", json!({ "path": "m.rs" })),
                ],
            ),
        ];
        assert_eq!(
            files_touched(&["z.rs".to_string(), "a.rs".to_string()], &history),
            ["a.rs", "b.rs", "m.rs", "z.rs"]
        );
    }
}
