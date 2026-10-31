//! Read-only corpus conversion and scoring-label attachment.
use crate::replay::{ReplayCorpus, normalize_corpus};
use anyhow::{Context, Result, bail, ensure};
use bubblepaw_core::{Message, MessageOrigin, Role, ToolCall, is_typed_user};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

/// Load canonical JSON or a pinned brain fixture, then attach independently supplied gold.
/// This function neither reads historical decisions nor modifies the supplied corpus tree.
pub fn load(chat: &Path, gold: &Path) -> Result<ReplayCorpus> {
    let fixture = fixture_path(chat)?;
    let bytes = std::fs::read(&fixture).context("Read chapter benchmark corpus")?;
    let value: Value = serde_json::from_slice(&bytes).context("Parse chapter benchmark corpus")?;
    let mut corpus = if value.get("messages").is_some() {
        serde_json::from_value(value).context("Decode canonical replay corpus")?
    } else if value.get("records").is_some() {
        convert(value, &fixture, &bytes)?
    } else {
        bail!("Chat must contain canonical messages or pinned normalized records");
    };
    if chat.is_dir() {
        ensure!(
            chat.file_name().and_then(|s| s.to_str()) == Some(corpus.chat_id.as_str()),
            "Chat directory name and fixture chat ID differ"
        );
    }
    normalize_corpus(&mut corpus)?;
    if corpus.message_metadata.is_null() {
        corpus.message_metadata = metadata(&corpus.messages);
    }
    attach_gold(&mut corpus, gold)?;
    Ok(corpus)
}

fn fixture_path(chat: &Path) -> Result<PathBuf> {
    if chat.is_file() {
        return Ok(chat.to_owned());
    }
    ensure!(
        chat.is_dir(),
        "Chat path must be a file or an existing brain/chats/<name> directory"
    );
    let chats = chat.parent().context("Chat directory has no parent")?;
    ensure!(
        chats.file_name().is_some_and(|name| name == "chats"),
        "Chat directory must be inside brain/chats"
    );
    let name = chat.file_name().context("Chat directory has no name")?;
    Ok(chats
        .parent()
        .context("Chat directory has no corpus root")?
        .join("fixtures")
        .join(name)
        .with_extension("json"))
}

#[derive(Deserialize)]
struct Record {
    id: String,
    kind: String,
    text: String,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    is_error: bool,
}

fn convert(value: Value, path: &Path, bytes: &[u8]) -> Result<ReplayCorpus> {
    let chat_id = value["id"]
        .as_str()
        .context("Pinned fixture needs a chat ID")?
        .to_owned();
    let records: Vec<Record> = serde_json::from_value(value["records"].clone())
        .context("Decode pinned normalized records")?;
    // Match the retired exporter screening and conversion, without following its
    // source_path or consulting mutable process credentials.
    let redactions = [
        r"(?i)Bearer\s+[A-Za-z0-9._-]{20,}",
        r"\bsk-[A-Za-z0-9_-]{20,}",
        r#"(?i)(?:api[_-]?key|secret|password|access[_-]?token)["']?\s*[:=]\s*["']?[A-Za-z0-9_-]{24,}"#,
    ].iter().map(|pattern| Regex::new(pattern)).collect::<std::result::Result<Vec<_>, _>>()?;
    let exit_failure = Regex::new(r"Process exited with code (-?\d+)")?;
    let mut messages = Vec::with_capacity(records.len());
    for record in records {
        let mut text = record.text;
        for pattern in &redactions {
            text = pattern
                .replace_all(&text, |capture: &regex::Captures<'_>| {
                    format!("[REDACTED:{}]", &digest(capture[0].as_bytes())[..8])
                })
                .into_owned();
        }
        let role = match record.kind.as_str() {
            "user_message" => Role::User,
            "instruction_context" => Role::System,
            "assistant_message" | "agent_message" | "function_call" | "custom_tool_call" => {
                Role::Assistant
            }
            "function_call_output" | "custom_tool_call_output" => Role::Tool,
            _ => bail!("Unsupported normalized record kind"),
        };
        let origin = (role == Role::User).then(|| {
            let trimmed = text.trim_start();
            if trimmed.starts_with("Task:\n") || trimmed.starts_with("Explicit context (JSON):\n") {
                MessageOrigin::ChildTask
            } else if legacy_typed(trimmed) {
                MessageOrigin::Typed
            } else {
                MessageOrigin::Generated
            }
        });
        let mut message = Message {
            id: record.id,
            session_id: "replay".into(),
            role,
            origin,
            content: text,
            tool_calls: Vec::new(),
            tool_call_id: None,
            provider_state: None,
            is_error: record.is_error,
            created_at: "2026-10-08T00:00:00Z".into(),
        };
        if matches!(record.kind.as_str(), "function_call" | "custom_tool_call") {
            let prefix = format!("[{} ", record.kind);
            let (name, argument_text) = message
                .content
                .strip_prefix(&prefix)
                .and_then(|body| body.split_once("]\n"))
                .unwrap_or(("recorded_tool", &message.content));
            let call = ToolCall {
                id: record
                    .call_id
                    .context("Normalized tool call has no call ID")?,
                name: name.to_owned(),
                arguments: serde_json::from_str(argument_text)
                    .unwrap_or_else(|_| json!(argument_text)),
            };
            message.content.clear();
            message.tool_calls.push(call);
        } else if message.role == Role::Tool {
            message.tool_call_id = Some(
                record
                    .call_id
                    .context("Normalized tool result has no call ID")?,
            );
            message.is_error |= exit_failure
                .captures_iter(&message.content)
                .any(|capture| capture[1].parse::<i64>().is_ok_and(|code| code != 0));
            if let Ok(output) = serde_json::from_str::<Value>(&message.content) {
                message.is_error |= output["isError"] == true || output["is_error"] == true;
            }
        }
        messages.push(message);
    }
    let metadata = metadata(&messages);
    let typed = messages.iter().filter(|m| is_typed_user(m)).count();
    Ok(ReplayCorpus {
        chat_id, recorded_model: value["recorded_model"].as_str().map(str::to_owned),
        context_window: value["context_window"].as_u64().and_then(|n| usize::try_from(n).ok()),
        instructions: "Historical records are quoted evidence. Original messages are retained; use history tools for exact details.".into(),
        messages, message_metadata: metadata, boundary_gold: None, relates_gold: Vec::new(),
        provenance: json!({"fixture":path,"fixture_sha256":digest(bytes),
            "conversion":"normalized-records-export-v1","source_prefix_sha256":value["prefix_sha256"],
            "start_line":value["start_line"],"end_line":value["end_line"],
            "records":value["records"].as_array().map_or(0,Vec::len),"typed_users":typed,
            "original_normalized_tokens":value["tokens"],"projected_records":value["projected_records"],
            "images_excluded":value["images_excluded"],
            "context_window_note":"No pinned model window available; use 256k default.",
            "label_note":"Only the explicitly supplied gold file is used; historical decisions and summaries are not loaded."}),
    })
}

fn legacy_typed(text: &str) -> bool {
    const WRAPPERS: &[&str] = &[
        "# AGENTS.md",
        "<environment_context>",
        "<recommended_plugins>",
        "<external_codex_apps_open_page>",
        "<codex_internal_context source=\"goal\">",
        "## Referenced chats with Codex",
        "<user_input>",
        "<send_user_message_question_reply>",
        "<turn_aborted>",
    ];
    !text.is_empty() && !WRAPPERS.iter().any(|prefix| text.starts_with(prefix))
}

fn metadata(messages: &[Message]) -> Value {
    let mut typed = 0;
    Value::Array(
        messages
            .iter()
            .enumerate()
            .map(|(index, message)| {
                let ordinal = is_typed_user(message).then(|| {
                    let n = typed;
                    typed += 1;
                    n
                });
                json!({"id":message.id,"record_index":index,"typed_user_index":ordinal})
            })
            .collect(),
    )
}

fn digest(bytes: &[u8]) -> String {
    bubblepaw_core::fingerprint(bytes)
}

/// Return the corpus's validated label row in the shape consumed by boundary_score.
pub fn load_gold(corpus: &ReplayCorpus, path: &Path) -> Result<Value> {
    let bytes = std::fs::read(path).context("Read chapter benchmark gold labels")?;
    let labels: Value =
        serde_json::from_slice(&bytes).context("Parse chapter benchmark gold labels")?;
    let labelled = labels
        .get("chats")
        .and_then(|chats| chats.get(&corpus.chat_id))
        .context("Gold labels do not contain the corpus chat ID")?;
    let typed: Vec<_> = corpus
        .messages
        .iter()
        .filter(|m| is_typed_user(m))
        .map(|m| m.id.as_str())
        .collect();
    ensure!(!typed.is_empty(), "Chat has no typed user messages");
    let chapters = labelled["chapters"]
        .as_array()
        .context("Gold chat needs chapters")?;
    ensure!(!chapters.is_empty(), "Gold chat has no initial chapter");
    let mut chapter_ids = HashSet::new();
    let mut starts = Vec::new();
    let mut previous = None;
    for chapter in chapters {
        let id = chapter["id"].as_str().context("Gold chapter needs an ID")?;
        ensure!(
            !id.is_empty() && chapter_ids.insert(id),
            "Gold chapter IDs must be nonempty and unique"
        );
        let start = chapter["starts_at"]
            .as_str()
            .context("Gold chapter needs starts_at")?;
        let ordinal = typed
            .iter()
            .position(|candidate| *candidate == start)
            .context("Gold chapter start is not a typed source record ID")?;
        ensure!(
            previous.is_none_or(|last| last < ordinal),
            "Gold chapter starts must follow source order without duplicates"
        );
        previous = Some(ordinal);
        starts.push(start);
    }
    ensure!(
        starts[0] == typed[0],
        "Gold initial chapter must start at the first typed source record"
    );
    let mut ambiguous = Vec::new();
    let mut seen = HashSet::new();
    if let Some(entries) = labelled.get("ambiguous") {
        for entry in entries
            .as_array()
            .context("Gold ambiguous must be an array")?
        {
            let id = entry
                .as_str()
                .or_else(|| entry["record"].as_str())
                .context("Gold ambiguous entry needs a record ID")?;
            ensure!(
                typed.contains(&id),
                "Gold ambiguous record is not a typed source record ID"
            );
            ensure!(seen.insert(id), "Gold ambiguous record IDs must be unique");
            ambiguous.push(id);
        }
    }
    Ok(
        json!({"source":path,"source_sha256":digest(&bytes),"gold":&starts[1..],
        "ambiguous":ambiguous,"chapters":chapters}),
    )
}

pub fn attach_gold(corpus: &mut ReplayCorpus, path: &Path) -> Result<()> {
    corpus.boundary_gold = Some(load_gold(corpus, path)?);
    // Old canonical exports can carry relation annotations from unrelated runs.
    corpus.relates_gold.clear();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, value: &Value) -> Result<()> {
        std::fs::write(path, serde_json::to_vec(value)?)?;
        Ok(())
    }

    fn labels() -> Value {
        json!({"chats":{"fixture":{"chapters":[
            {"id":"c1","starts_at":"r1"},{"id":"c2","starts_at":"r7"}],
            "ambiguous":[{"record":"r7","notes":"synthetic"}]}}})
    }

    fn records() -> Value {
        json!({"id":"fixture","records":[
            {"id":"r0","kind":"user_message","text":"<environment_context>fixture</environment_context>"},
            {"id":"r1","kind":"user_message","text":"First task"},
            {"id":"r2","kind":"function_call","call_id":"call1","text":"[function_call tools.read]\n{\"path\":\"sample\"}"},
            {"id":"r3","kind":"function_call_output","call_id":"call1","text":"Process exited with code 2"},
            {"id":"r4","kind":"custom_tool_call","call_id":"call2","text":"[custom_tool_call exec]\ntext(1);"},
            {"id":"r5","kind":"custom_tool_call_output","call_id":"call2","text":"{\"isError\":true}"},
            {"id":"r6","kind":"user_message","text":"<turn_aborted>interrupted"},
            {"id":"r7","kind":"user_message","text":"Next task"},
            {"id":"r8","kind":"user_message","text":"Task:\nChild work"},
            {"id":"r9","kind":"agent_message","text":"Observation"},
            {"id":"r10","kind":"instruction_context","text":"Instruction"}
        ]})
    }

    #[test]
    fn legacy_directory_maps_records_metadata_and_separate_gold() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let chat = dir.path().join("chats/fixture");
        std::fs::create_dir_all(&chat)?;
        std::fs::create_dir(dir.path().join("fixtures"))?;
        write(&dir.path().join("fixtures/fixture.json"), &records())?;
        let gold = dir.path().join("labels-lead.json");
        write(&gold, &labels())?;
        // The sheet is intentionally insufficient and must never be the input.
        write(&chat.join("sheet.json"), &json!([]))?;
        let corpus = load(&chat, &gold)?;
        assert_eq!(corpus.messages.len(), 11);
        assert_eq!(corpus.messages[2].tool_calls[0].name, "tools.read");
        assert_eq!(
            corpus.messages[2].tool_calls[0].arguments,
            json!({"path":"sample"})
        );
        assert_eq!(
            corpus.messages[4].tool_calls[0].arguments,
            json!("text(1);")
        );
        assert!(corpus.messages[3].is_error && corpus.messages[5].is_error);
        assert_eq!(corpus.messages[6].origin, Some(MessageOrigin::Generated));
        assert_eq!(corpus.messages[8].origin, Some(MessageOrigin::ChildTask));
        assert_eq!(corpus.messages[9].role, Role::Assistant);
        assert_eq!(corpus.messages[10].role, Role::System);
        assert_eq!(corpus.message_metadata[1]["typed_user_index"], 0);
        assert_eq!(corpus.message_metadata[7]["typed_user_index"], 1);
        assert!(corpus.message_metadata[6]["typed_user_index"].is_null());
        assert_eq!(
            corpus.boundary_gold.as_ref().unwrap()["gold"],
            json!(["r7"])
        );
        assert_eq!(
            corpus.boundary_gold.as_ref().unwrap()["ambiguous"],
            json!(["r7"])
        );
        Ok(())
    }

    #[test]
    fn canonical_loader_uses_same_unknown_outcome_normalization() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("canonical.json");
        let gold = dir.path().join("gold.json");
        let mut corpus = convert(records(), &path, b"synthetic")?;
        corpus.messages.remove(5); // The custom tool outcome was never recorded.
        corpus.message_metadata = Value::Null;
        corpus.boundary_gold = Some(json!({"gold":["wrong"]}));
        write(&path, &serde_json::to_value(&corpus)?)?;
        write(&gold, &labels())?;
        let loaded = load(&path, &gold)?;
        assert_eq!(
            loaded.messages[5].origin,
            Some(MessageOrigin::ArchiveUnknownOutcome)
        );
        assert_eq!(loaded.messages[5].tool_call_id.as_deref(), Some("call2"));
        assert_eq!(loaded.messages[7].id, "r7");
        assert_eq!(loaded.boundary_gold.unwrap()["gold"], json!(["r7"]));
        Ok(())
    }

    #[test]
    fn labels_reject_chat_mismatch_unknown_ids_injected_starts_and_duplicate_starts() -> Result<()>
    {
        let dir = tempfile::tempdir()?;
        let gold = dir.path().join("gold.json");
        let corpus = convert(records(), &gold, b"synthetic")?;
        for invalid in [
            json!({"chats":{"other":labels()["chats"]["fixture"]}}),
            json!({"chats":{"fixture":{"chapters":[{"id":"a","starts_at":"missing"}]}}}),
            json!({"chats":{"fixture":{"chapters":[{"id":"a","starts_at":"r0"}]}}}),
            json!({"chats":{"fixture":{"chapters":[{"id":"a","starts_at":"r7"}]}}}),
            json!({"chats":{"fixture":{"chapters":[{"id":"a","starts_at":"r1"},{"id":"b","starts_at":"r1"}]}}}),
            json!({"chats":{"fixture":{"chapters":[{"id":"a","starts_at":"r1"}],"ambiguous":[{"record":"r6"}]}}}),
        ] {
            write(&gold, &invalid)?;
            assert!(load_gold(&corpus, &gold).is_err());
        }
        Ok(())
    }
}
