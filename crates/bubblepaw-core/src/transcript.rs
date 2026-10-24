//! Bounded, offline imports of native coding-agent transcripts.
//!
//! Only canonical response items are messages. Repeated progress, request views,
//! context wrappers and usage events are counted rather than copied. Source
//! compactions are imported derived records. Every reasoning artifact, including
//! opaque ciphertext and signatures, stays once in canonical assistant state.
use crate::{
    ContextStrategy, Message, MessageOrigin, Role, Store, ToolCall, with_provider_state_origin,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

pub const IMPORT_CONVERTER: &str = "bubblepaw-transcript-v2";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImportFormat {
    Codex,
    Claude,
}

#[derive(Debug, Clone)]
pub struct ImportOptions {
    pub title: Option<String>,
    pub batch_messages: usize,
    pub batch_bytes: usize,
    /// A source line exceeding this limit fails before creating any chat.
    pub max_line_bytes: usize,
    pub adjacent_agent_paths: Vec<PathBuf>,
    /// Discover linked Codex siblings or Claude's native subagents directory.
    pub discover_adjacent_agents: bool,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            title: None,
            batch_messages: 128,
            batch_bytes: 2 * 1024 * 1024,
            max_line_bytes: 64 * 1024 * 1024,
            adjacent_agent_paths: Vec::new(),
            discover_adjacent_agents: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportStats {
    pub source_lines: u64,
    pub source_bytes: u64,
    pub messages: u64,
    pub user_messages: u64,
    pub assistant_messages: u64,
    pub tool_messages: u64,
    pub tool_calls: u64,
    pub derived_records: u64,
    pub compactions: u64,
    pub reasoning_artifacts: u64,
    pub opaque_encrypted_fields: u64,
    /// Compatibility counter from converter v1. Converter v2 never drops fields.
    pub encrypted_fields_dropped: u64,
    pub excluded: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImportDerivedRecord {
    pub id: String,
    pub session_id: String,
    pub source_line: u64,
    pub source_kind: String,
    pub through_message_id: Option<String>,
    pub content: Value,
    pub created_at: String,
    pub origin: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportedSource {
    pub session_id: String,
    pub source_path: String,
    pub source_sha256: String,
    pub source_session_id: Option<String>,
    pub source_version: Option<String>,
    pub source_parent_session_id: Option<String>,
    pub stats: ImportStats,
    pub normalized_sha256: String,
    pub normalized_derived_sha256: String,
    pub reasoning_artifact_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportResult {
    pub root_session_id: String,
    pub sources: Vec<ImportedSource>,
    pub stats: ImportStats,
}

struct Inventory {
    path: PathBuf,
    sha256: String,
    bytes: u64,
    lines: u64,
    session_id: Option<String>,
    source_version: Option<String>,
    parent_source_id: Option<String>,
    typed_lines: HashMap<u64, bool>,
    typed_events: u64,
}

struct Lines {
    reader: BufReader<File>,
    limit: usize,
    number: u64,
    bytes: u64,
    hash: Sha256,
    buffer: Vec<u8>,
}

impl Lines {
    fn open(path: &Path, limit: usize) -> Result<Self> {
        Ok(Self {
            reader: BufReader::with_capacity(64 * 1024, File::open(path)?),
            limit,
            number: 0,
            bytes: 0,
            hash: Sha256::new(),
            buffer: Vec::new(),
        })
    }

    fn next(&mut self) -> Result<Option<Value>> {
        self.buffer.clear();
        loop {
            let available = self.reader.fill_buf()?;
            if available.is_empty() {
                break;
            }
            let size = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |index| index + 1);
            ensure!(
                self.buffer.len().saturating_add(size) <= self.limit,
                "Transcript line {} exceeds configured byte limit",
                self.number + 1
            );
            let finished = available[size - 1] == b'\n';
            self.buffer.extend_from_slice(&available[..size]);
            self.reader.consume(size);
            if finished {
                break;
            }
        }
        if self.buffer.is_empty() {
            return Ok(None);
        }
        self.number += 1;
        self.bytes += self.buffer.len() as u64;
        self.hash.update(&self.buffer);
        // Do not propagate serde's error: it can include private source tokens.
        let value = serde_json::from_slice(&self.buffer)
            .map_err(|_| anyhow::anyhow!("Invalid transcript JSON at line {}", self.number))?;
        Ok(Some(value))
    }

    fn sha256(&self) -> String {
        format!("{:x}", self.hash.clone().finalize())
    }
}

/// Import a root and explicitly supplied or adjacent Claude subagent transcripts.
/// All sources are fully validated before any session is created. Failed writes
/// remove the new tree; the caller's existing chats and source files are untouched.
pub fn import_file(
    store: &Store,
    source: impl AsRef<Path>,
    format: ImportFormat,
    options: &ImportOptions,
) -> Result<ImportResult> {
    ensure!(
        options.batch_messages > 0 && options.batch_bytes > 0 && options.max_line_bytes > 0,
        "Import batch and line limits must be positive"
    );
    let source = source.as_ref().canonicalize()?;
    let mut paths = vec![source.clone()];
    paths.extend(
        options
            .adjacent_agent_paths
            .iter()
            .map(|path| path.canonicalize())
            .collect::<std::io::Result<Vec<_>>>()?,
    );
    if format == ImportFormat::Claude && options.discover_adjacent_agents {
        let agents = source.with_extension("").join("subagents");
        if agents.is_dir() {
            let mut discovered = Vec::new();
            for entry in std::fs::read_dir(agents)? {
                let path = entry?.path();
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("");
                if path.is_file() && name.starts_with("agent-") && name.ends_with(".jsonl") {
                    discovered.push(path.canonicalize()?);
                }
            }
            discovered.sort();
            paths.extend(discovered);
        }
    }
    if format == ImportFormat::Codex
        && options.discover_adjacent_agents
        && let Some((root_source_id, _)) = codex_header(&source, options.max_line_bytes)
        && let Some(directory) = source.parent()
    {
        let mut candidates = Vec::new();
        for entry in std::fs::read_dir(directory)? {
            let path = entry?.path();
            if path.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
            {
                let path = path.canonicalize()?;
                if path == source {
                    continue;
                }
                if let Some((id, Some(parent))) = codex_header(&path, options.max_line_bytes) {
                    candidates.push((path, id, parent));
                }
            }
        }
        candidates.sort_by(|left, right| left.0.cmp(&right.0));
        let mut reachable = HashSet::from([root_source_id]);
        let mut selected = HashSet::new();
        loop {
            let mut changed = false;
            for (path, id, parent) in &candidates {
                if reachable.contains(parent) && selected.insert(path.clone()) {
                    reachable.insert(id.clone());
                    paths.push(path.clone());
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }
    let mut unique = HashSet::new();
    paths.retain(|path| unique.insert(path.clone()));
    let inventories = paths
        .iter()
        .map(|path| inventory(path, format, options.max_line_bytes))
        .collect::<Result<Vec<_>>>()?;
    // Normalizing into a discard callback catches unsupported content before any
    // durable mutation and measures the exact expected normalized export hash.
    let previews = inventories
        .iter()
        .map(|item| normalize(item, format, options, "preview", |_, _| Ok(())))
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        previews
            .iter()
            .all(|preview| preview.stats.messages > 0 || preview.stats.derived_records > 0),
        "Transcript has no importable native content in the selected format"
    );
    let known_sources: HashSet<_> = inventories
        .iter()
        .filter_map(|item| item.session_id.as_ref())
        .collect();
    let mut bound_sources: HashSet<_> = inventories[0].session_id.iter().collect();
    let mut pending: Vec<_> = (1..inventories.len()).collect();
    let mut source_order = vec![0];
    while !pending.is_empty() {
        let position = pending
            .iter()
            .position(|index| {
                inventories[*index]
                    .parent_source_id
                    .as_ref()
                    .is_none_or(|parent| {
                        !known_sources.contains(parent) || bound_sources.contains(parent)
                    })
            })
            .context("Supplied native agent transcripts contain a parent cycle")?;
        let index = pending.remove(position);
        if let Some(id) = &inventories[index].session_id {
            bound_sources.insert(id);
        }
        source_order.push(index);
    }
    let title = options.title.clone().unwrap_or_else(|| {
        source
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("Imported chat")
            .to_owned()
    });
    let root = store.create_session_with_parent(title, ContextStrategy::default(), None)?;
    let outcome = (|| -> Result<ImportResult> {
        let mut sessions = HashMap::new();
        if let Some(id) = &inventories[0].session_id {
            sessions.insert(id.clone(), root.id.clone());
        }
        let mut result = ImportResult {
            root_session_id: root.id.clone(),
            sources: Vec::new(),
            stats: ImportStats::default(),
        };
        for index in source_order {
            let item = &inventories[index];
            let session_id = if index == 0 {
                root.id.clone()
            } else {
                let parent = item
                    .parent_source_id
                    .as_ref()
                    .and_then(|id| sessions.get(id))
                    .unwrap_or(&root.id);
                store
                    .create_session_with_parent(
                        format!(
                            "Imported agent {}",
                            item.path
                                .file_stem()
                                .and_then(|name| name.to_str())
                                .unwrap_or("child")
                        ),
                        ContextStrategy::default(),
                        Some(parent),
                    )?
                    .id
            };
            if let Some(id) = &item.session_id {
                // Claude subagent files may share their parent's sessionId. The
                // native path is a distinct session even when that ID is shared.
                sessions
                    .entry(id.clone())
                    .or_insert_with(|| session_id.clone());
            }
            let imported = normalize(item, format, options, &session_id, |messages, derived| {
                store.append_import_batch(messages, derived)
            })?;
            ensure!(
                imported.stats == previews[index].stats
                    && imported.normalized_sha256 == previews[index].normalized_sha256
                    && imported.normalized_derived_sha256
                        == previews[index].normalized_derived_sha256,
                "Transcript normalization changed after validation"
            );
            store.set_import_metadata(&session_id, &json!({
                "converter": IMPORT_CONVERTER, "format": format,
                "source_path": imported.source_path, "source_sha256": imported.source_sha256,
                "source_bytes": item.bytes, "source_session_id": imported.source_session_id,
                "source_version": imported.source_version, "source_parent_session_id": imported.source_parent_session_id,
                "stats": imported.stats, "normalized_sha256": imported.normalized_sha256,
                "normalized_derived_sha256": imported.normalized_derived_sha256,
                "reasoning_artifact_sha256": imported.reasoning_artifact_sha256,
                "encrypted_reasoning_policy": "preserve_canonical", "status": "complete"
            }))?;
            add_stats(&mut result.stats, &imported.stats);
            result.sources.push(imported);
        }
        store.idle_checkpoint()?;
        Ok(result)
    })();
    if outcome.is_err() {
        store
            .delete_session(&root.id)
            .context("Failed to roll back a new transcript import")?;
    }
    outcome
}

fn inventory(path: &Path, format: ImportFormat, limit: usize) -> Result<Inventory> {
    let mut lines = Lines::open(path, limit)?;
    let mut result = Inventory {
        path: path.to_owned(),
        sha256: String::new(),
        bytes: 0,
        lines: 0,
        session_id: None,
        source_version: None,
        parent_source_id: None,
        typed_lines: HashMap::new(),
        typed_events: 0,
    };
    let mut prior = VecDeque::new();
    while let Some(event) = lines.next()? {
        let kind = string(&event, "type");
        if format == ImportFormat::Claude {
            if result.source_version.is_none() {
                result.source_version = event
                    .get("version")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            if result.session_id.is_none() {
                result.session_id = event
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            continue;
        }
        let payload = &event["payload"];
        if kind == "session_meta" {
            result.source_version = payload
                .get("cli_version")
                .and_then(Value::as_str)
                .map(str::to_owned);
            result.session_id = payload
                .get("session_id")
                .or_else(|| payload.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            result.parent_source_id = codex_parent(payload);
        }
        if kind == "response_item"
            && string(payload, "type") == "message"
            && string(payload, "role") == "user"
        {
            let text = text(&payload["content"]);
            prior.push_back((
                lines.number,
                identity(&text),
                identity(&strip_injected(&text)),
            ));
            while prior.len() > 64 {
                prior.pop_front();
            }
        }
        let item = &payload["item"];
        let typed = kind == "event_msg"
            && ((string(payload, "type") == "item_completed"
                && string(item, "type") == "UserMessage")
                || string(payload, "type") == "user_message");
        if typed {
            result.typed_events += 1;
            let value = if string(payload, "type") == "user_message" {
                &payload["message"]
            } else {
                &item["content"]
            };
            let original = text(value);
            let raw = identity(&original);
            let stripped = identity(&strip_injected(&original));
            let found = prior.iter().rev().find(|(line, candidate, clean)| {
                !result.typed_lines.contains_key(line)
                    && (candidate == &raw
                        || clean == &raw
                        || (!stripped.is_empty() && clean == &stripped))
            });
            if let Some((line, candidate, _)) = found {
                result.typed_lines.insert(*line, candidate == &raw);
            } else if !original.is_empty() {
                bail!(
                    "Unmatched original user event at source line {}",
                    lines.number
                );
            }
        }
    }
    ensure!(lines.number > 0, "Transcript is empty");
    result.sha256 = lines.sha256();
    result.bytes = lines.bytes;
    result.lines = lines.number;
    Ok(result)
}

fn codex_parent(payload: &Value) -> Option<String> {
    payload
        .pointer("/source/subagent/thread_spawn/parent_thread_id")
        .or_else(|| payload.pointer("/source/subagent/parent_thread_id"))
        .or_else(|| payload.get("parent_thread_id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Native Codex begins with session_meta. Adjacent files of other formats or
/// invalid headers are ignored, rather than interpreting their content as agents.
/// Each peek reads at most one bounded line and retains only IDs and filenames.
fn codex_header(path: &Path, limit: usize) -> Option<(String, Option<String>)> {
    let mut lines = Lines::open(path, limit).ok()?;
    let event = lines.next().ok()??;
    if string(&event, "type") != "session_meta" {
        return None;
    }
    let payload = &event["payload"];
    let id = payload
        .get("session_id")
        .or_else(|| payload.get("id"))?
        .as_str()?
        .to_owned();
    Some((id, codex_parent(payload)))
}

struct Normalizer<'a> {
    inventory: &'a Inventory,
    session: &'a str,
    stats: ImportStats,
    messages: Vec<Message>,
    derived: Vec<ImportDerivedRecord>,
    pending_bytes: usize,
    last_message: Option<String>,
    digest: Sha256,
    derived_digest: Sha256,
    reasoning_digest: Sha256,
    current_model: Option<String>,
}

impl Normalizer<'_> {
    fn exclude(&mut self, kind: &str) {
        *self.stats.excluded.entry(kind.into()).or_default() += 1;
    }

    #[allow(clippy::too_many_arguments)]
    fn message(
        &mut self,
        line: u64,
        ordinal: usize,
        source_kind: &str,
        event: &Value,
        role: Role,
        content: String,
        calls: Vec<ToolCall>,
        call_id: Option<String>,
        error: bool,
    ) -> Result<()> {
        self.message_with_state(
            line,
            ordinal,
            source_kind,
            event,
            role,
            content,
            calls,
            call_id,
            error,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn message_with_state(
        &mut self,
        line: u64,
        ordinal: usize,
        source_kind: &str,
        event: &Value,
        role: Role,
        content: String,
        calls: Vec<ToolCall>,
        call_id: Option<String>,
        error: bool,
        provider_state: Option<Value>,
    ) -> Result<()> {
        let mut state = provider_state.unwrap_or_else(|| json!({}));
        state["import"] = json!({"source_line":line,"source_kind":source_kind,
            "source_id":event.get("uuid").or_else(||event.pointer("/payload/id")),
            "converter":IMPORT_CONVERTER,
            "phase":event.pointer("/payload/phase"), "namespace":event.pointer("/payload/namespace"),
            "author":event.pointer("/payload/author"), "recipient":event.pointer("/payload/recipient")});
        if source_kind == "custom_tool_call" {
            state["import"]["native_call_extras"] = native_call_extras(&event["payload"]);
        }
        let message = Message {
            id: format!("{}-l{line}-m{ordinal:06}", self.session),
            session_id: self.session.into(),
            origin: (role == Role::User).then_some(MessageOrigin::Typed),
            role: role.clone(),
            content,
            tool_calls: calls,
            tool_call_id: call_id,
            is_error: error,
            provider_state: Some(state),
            created_at: timestamp(event),
        };
        self.stats.messages += 1;
        self.stats.tool_calls += message.tool_calls.len() as u64;
        match role {
            Role::User => self.stats.user_messages += 1,
            Role::Assistant => self.stats.assistant_messages += 1,
            Role::Tool => self.stats.tool_messages += 1,
            Role::System => {}
        }
        let normalized = normalized_message(&message);
        self.hash("message", &normalized)?;
        self.pending_bytes = self
            .pending_bytes
            .saturating_add(serde_json::to_vec(&message)?.len());
        self.last_message = Some(message.id.clone());
        self.messages.push(message);
        Ok(())
    }

    fn reasoning(
        &mut self,
        line: u64,
        ordinal: usize,
        source_kind: &str,
        event: &Value,
        format: ImportFormat,
        artifact: &Value,
    ) -> Result<()> {
        let (kind, source, envelope) = match format {
            ImportFormat::Codex => (
                "responses",
                "import:codex",
                json!({"adapter":"responses","items":[artifact]}),
            ),
            ImportFormat::Claude => (
                "anthropic-messages",
                "import:claude-code",
                json!({"adapter":"anthropic-messages","content":[artifact]}),
            ),
        };
        let state =
            with_provider_state_origin(envelope, kind, self.current_model.as_deref(), source)?;
        self.stats.reasoning_artifacts += 1;
        self.stats.opaque_encrypted_fields += encrypted_field_count(artifact);
        self.reasoning_digest.update(serde_json::to_vec(artifact)?);
        self.reasoning_digest.update(b"\n");
        self.message_with_state(
            line,
            ordinal,
            source_kind,
            event,
            Role::Assistant,
            String::new(),
            Vec::new(),
            None,
            false,
            Some(state),
        )
    }

    fn derived(
        &mut self,
        line: u64,
        ordinal: usize,
        kind: &str,
        event: &Value,
        content: Value,
    ) -> Result<()> {
        let record = ImportDerivedRecord {
            id: format!("{}-l{line}-d{ordinal:06}", self.session),
            session_id: self.session.into(),
            source_line: line,
            source_kind: kind.into(),
            through_message_id: self.last_message.clone(),
            content,
            created_at: timestamp(event),
            origin: "import".into(),
        };
        self.stats.derived_records += 1;
        self.hash("derived", &normalized_derived(&record))?;
        self.pending_bytes = self
            .pending_bytes
            .saturating_add(serde_json::to_vec(&record)?.len());
        self.derived.push(record);
        Ok(())
    }

    fn hash(&mut self, kind: &str, value: &Value) -> Result<()> {
        let digest = if kind == "message" {
            &mut self.digest
        } else {
            &mut self.derived_digest
        };
        digest.update(serde_json::to_vec(value)?);
        digest.update(b"\n");
        Ok(())
    }

    fn codex(&mut self, line: u64, event: &Value) -> Result<()> {
        let kind = string(event, "type");
        let payload = &event["payload"];
        if matches!(kind, "session_meta" | "turn_context")
            && let Some(model) = payload.get("model").and_then(Value::as_str)
        {
            self.current_model = Some(model.to_owned());
        }
        if kind == "compacted" {
            let mut kept = payload.clone();
            if let Some(map) = kept.as_object_mut() {
                for field in ["replacement_history", "guardian_history"] {
                    if map.remove(field).is_some() {
                        self.exclude(&format!("compaction_{field}_copy"));
                    }
                }
                map.remove("latest_token_usage_record");
            }
            self.stats.compactions += 1;
            return self.derived(line, 0, "codex_compaction", event, kept);
        }
        if kind != "response_item" {
            self.exclude(kind);
            return Ok(());
        }
        let item_kind = string(payload, "type");
        match item_kind {
            "message" => {
                let role = string(payload, "role");
                if !matches!(role, "user" | "assistant") {
                    self.exclude("harness_role");
                    return Ok(());
                }
                let mut content = payload["content"].clone();
                if role == "user" {
                    if self.inventory.typed_events > 0 {
                        let Some(preserve) = self.inventory.typed_lines.get(&line) else {
                            self.exclude("non_original_user");
                            return Ok(());
                        };
                        if !preserve {
                            clean_content(&mut content);
                        }
                    } else {
                        clean_content(&mut content);
                    }
                    if content_empty(&content) {
                        self.exclude("harness_user");
                        return Ok(());
                    }
                }
                self.message(
                    line,
                    0,
                    "codex_message",
                    event,
                    if role == "user" {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    content_string(&content)?,
                    Vec::new(),
                    None,
                    false,
                )
            }
            "function_call" | "custom_tool_call" => {
                let mut args = payload
                    .get("arguments")
                    .or_else(|| payload.get("input"))
                    .cloned()
                    .unwrap_or(Value::Null);
                if item_kind == "function_call"
                    && let Some(raw) = args.as_str()
                {
                    args = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.into()));
                }
                ensure!(
                    !args.is_null(),
                    "Missing native tool arguments at source line {line}"
                );
                let name = match payload.get("namespace").and_then(Value::as_str) {
                    Some(namespace) if !namespace.is_empty() => {
                        format!("{namespace}.{}", string(payload, "name"))
                    }
                    _ => string(payload, "name").to_owned(),
                };
                let call = ToolCall {
                    id: required(payload, "call_id", line)?.into(),
                    name,
                    arguments: args,
                };
                let mut block = native_call_extras(payload);
                block["type"] = json!(item_kind);
                let envelope = if item_kind == "function_call" {
                    json!({"adapter":"responses","format":"message-references-v1","items":[block],"refs":[{"call":0}]})
                } else {
                    json!({"adapter":"responses"})
                };
                let state = with_provider_state_origin(
                    envelope,
                    "responses",
                    self.current_model.as_deref(),
                    "import:codex",
                )?;
                self.stats.opaque_encrypted_fields += encrypted_field_count(payload);
                self.message_with_state(
                    line,
                    0,
                    item_kind,
                    event,
                    Role::Assistant,
                    String::new(),
                    vec![call],
                    None,
                    false,
                    Some(state),
                )
            }
            "function_call_output" | "custom_tool_call_output" => self.message(
                line,
                0,
                item_kind,
                event,
                Role::Tool,
                content_string(&payload["output"])?,
                Vec::new(),
                Some(required(payload, "call_id", line)?.into()),
                false,
            ),
            "reasoning" => self.reasoning(
                line,
                0,
                "codex_reasoning",
                event,
                ImportFormat::Codex,
                payload,
            ),
            "agent_message" => self.message(
                line,
                0,
                item_kind,
                event,
                Role::Assistant,
                content_string(&payload["content"])?,
                Vec::new(),
                None,
                false,
            ),
            "web_search_call" | "tool_search_call" | "tool_search_output" => {
                let mut value = payload.clone();
                if let Some(map) = value.as_object_mut() {
                    map.remove("internal_chat_message_metadata_passthrough");
                }
                self.message(
                    line,
                    0,
                    item_kind,
                    event,
                    Role::Assistant,
                    serde_json::to_string(&value)?,
                    Vec::new(),
                    None,
                    false,
                )
            }
            // An unfamiliar response item may carry real content. Preserve its
            // structure as source-derived evidence instead of guessing a role.
            _ => self.derived(line, 0, "codex_native_item", event, payload.clone()),
        }
    }

    fn claude(&mut self, line: u64, event: &Value) -> Result<()> {
        let kind = string(event, "type");
        if let Some(model) = event.pointer("/message/model").and_then(Value::as_str) {
            self.current_model = Some(model.to_owned());
        }
        if kind == "system" && string(event, "subtype") == "compact_boundary" {
            self.stats.compactions += 1;
            return self.derived(
                line,
                0,
                "claude_compaction_boundary",
                event,
                json!({"content":event["content"],"compactMetadata":event["compactMetadata"]}),
            );
        }
        if kind == "summary" || event["isCompactSummary"] == true {
            let value = event.get("message").cloned().unwrap_or_else(
                || json!({"content":event.get("summary").or_else(||event.get("content"))}),
            );
            return self.derived(line, 0, "claude_compaction_summary", event, value);
        }
        if kind == "attachment" {
            let attachment = &event["attachment"];
            if matches!(string(attachment, "type"), "file" | "edited_text_file") {
                return self.derived(
                    line,
                    0,
                    "claude_source_attachment",
                    event,
                    attachment.clone(),
                );
            }
            self.exclude("harness_attachment");
            return Ok(());
        }
        if !matches!(kind, "assistant" | "user") {
            self.exclude(kind);
            return Ok(());
        }
        let content = &event["message"]["content"];
        if event["isMeta"] == true || (kind == "user" && string(event, "promptSource") == "system")
        {
            self.exclude("harness_user");
            return Ok(());
        }
        if kind == "user" {
            if let Some(blocks) = content.as_array() {
                let mut ordinary = Vec::new();
                let mut ordinal = 0;
                for block in blocks {
                    if string(block, "type") == "tool_result" {
                        if !ordinary.is_empty() {
                            self.claude_user(
                                line,
                                ordinal,
                                event,
                                Value::Array(std::mem::take(&mut ordinary)),
                            )?;
                            ordinal += 1;
                        }
                        self.message(
                            line,
                            ordinal,
                            "claude_tool_result",
                            event,
                            Role::Tool,
                            content_string(&block["content"])?,
                            Vec::new(),
                            Some(required(block, "tool_use_id", line)?.into()),
                            block["is_error"] == true,
                        )?;
                        ordinal += 1;
                    } else {
                        ordinary.push(block.clone());
                    }
                }
                if !ordinary.is_empty() {
                    self.claude_user(line, ordinal, event, Value::Array(ordinary))?;
                }
            } else {
                self.claude_user(line, 0, event, content.clone())?;
            }
            return Ok(());
        }
        let blocks = content
            .as_array()
            .context("Native assistant content must be an array")?;
        let mut ordinary = Vec::new();
        let mut calls = Vec::new();
        let mut metadata = Vec::new();
        for (index, block) in blocks.iter().enumerate() {
            match string(block, "type") {
                "thinking" | "redacted_thinking" => self.reasoning(
                    line,
                    index,
                    if string(block, "type") == "thinking" {
                        "claude_thinking"
                    } else {
                        "claude_redacted_thinking"
                    },
                    event,
                    ImportFormat::Claude,
                    block,
                )?,
                "tool_use" => {
                    calls.push(ToolCall {
                        id: required(block, "id", line)?.into(),
                        name: required(block, "name", line)?.into(),
                        arguments: block["input"].clone(),
                    });
                    let mut meta = block.clone();
                    if let Some(map) = meta.as_object_mut() {
                        for field in ["type", "id", "name", "input"] {
                            map.remove(field);
                        }
                    }
                    if meta.as_object().is_some_and(|map| !map.is_empty()) {
                        metadata.push(json!({"call_id":block["id"],"metadata":meta}));
                    }
                }
                _ => ordinary.push(block.clone()),
            }
        }
        if !metadata.is_empty() {
            self.derived(
                line,
                blocks.len(),
                "claude_tool_metadata",
                event,
                metadata.into(),
            )?;
        }
        if !ordinary.is_empty() || !calls.is_empty() {
            self.message(
                line,
                blocks.len(),
                "claude_assistant",
                event,
                Role::Assistant,
                content_string(&Value::Array(ordinary))?,
                calls,
                None,
                event["isApiErrorMessage"] == true,
            )?;
        }
        Ok(())
    }

    fn claude_user(
        &mut self,
        line: u64,
        ordinal: usize,
        event: &Value,
        mut content: Value,
    ) -> Result<()> {
        // Explicit SDK/human prompts take precedence over harness-looking text.
        // Older transcripts lack that flag; remove only known ambient wrappers.
        if string(event, "promptSource") != "sdk" && string(event, "promptSource") != "human" {
            clean_content(&mut content);
        }
        if content_empty(&content) {
            self.exclude("harness_user");
            return Ok(());
        }
        self.message(
            line,
            ordinal,
            "claude_user",
            event,
            Role::User,
            content_string(&content)?,
            Vec::new(),
            None,
            false,
        )
    }
}

fn normalize(
    inventory: &Inventory,
    format: ImportFormat,
    options: &ImportOptions,
    session: &str,
    mut commit: impl FnMut(&[Message], &[ImportDerivedRecord]) -> Result<()>,
) -> Result<ImportedSource> {
    let mut lines = Lines::open(&inventory.path, options.max_line_bytes)?;
    let mut normalizer = Normalizer {
        inventory,
        session,
        stats: ImportStats::default(),
        messages: Vec::new(),
        derived: Vec::new(),
        pending_bytes: 0,
        last_message: None,
        digest: Sha256::new(),
        derived_digest: Sha256::new(),
        reasoning_digest: Sha256::new(),
        current_model: None,
    };
    while let Some(event) = lines.next()? {
        match format {
            ImportFormat::Codex => normalizer.codex(lines.number, &event)?,
            ImportFormat::Claude => normalizer.claude(lines.number, &event)?,
        }
        if normalizer.messages.len() + normalizer.derived.len() >= options.batch_messages
            || normalizer.pending_bytes >= options.batch_bytes
        {
            commit(&normalizer.messages, &normalizer.derived)?;
            normalizer.messages.clear();
            normalizer.derived.clear();
            normalizer.pending_bytes = 0;
        }
    }
    ensure!(
        lines.sha256() == inventory.sha256
            && lines.bytes == inventory.bytes
            && lines.number == inventory.lines,
        "Transcript changed during import; new chat must not be used"
    );
    commit(&normalizer.messages, &normalizer.derived)?;
    normalizer.stats.source_lines = lines.number;
    normalizer.stats.source_bytes = lines.bytes;
    Ok(ImportedSource {
        session_id: session.into(),
        source_path: inventory.path.to_string_lossy().into_owned(),
        source_sha256: inventory.sha256.clone(),
        source_session_id: inventory.session_id.clone(),
        source_version: inventory.source_version.clone(),
        source_parent_session_id: inventory.parent_source_id.clone(),
        stats: normalizer.stats,
        normalized_sha256: format!("{:x}", normalizer.digest.finalize()),
        normalized_derived_sha256: format!("{:x}", normalizer.derived_digest.finalize()),
        reasoning_artifact_sha256: format!("{:x}", normalizer.reasoning_digest.finalize()),
    })
}

/// Stable content projection used to compare preflight normalization with export.
/// Generated store/session IDs are intentionally excluded.
pub fn normalized_message(message: &Message) -> Value {
    json!({"role":message.role,"origin":message.origin,"content":message.content,"tool_calls":message.tool_calls,
        "tool_call_id":message.tool_call_id,"provider_state":message.provider_state,"is_error":message.is_error,"created_at":message.created_at})
}

pub fn normalized_derived(record: &ImportDerivedRecord) -> Value {
    json!({"source_line":record.source_line,"source_kind":record.source_kind,"content":record.content,
        "created_at":record.created_at,"origin":record.origin})
}

fn add_stats(total: &mut ImportStats, stats: &ImportStats) {
    total.source_lines += stats.source_lines;
    total.source_bytes += stats.source_bytes;
    total.messages += stats.messages;
    total.user_messages += stats.user_messages;
    total.assistant_messages += stats.assistant_messages;
    total.tool_messages += stats.tool_messages;
    total.tool_calls += stats.tool_calls;
    total.derived_records += stats.derived_records;
    total.compactions += stats.compactions;
    total.reasoning_artifacts += stats.reasoning_artifacts;
    total.opaque_encrypted_fields += stats.opaque_encrypted_fields;
    total.encrypted_fields_dropped += stats.encrypted_fields_dropped;
    for (kind, count) in &stats.excluded {
        *total.excluded.entry(kind.clone()).or_default() += count;
    }
}

fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}
fn required<'a>(value: &'a Value, key: &str, line: u64) -> Result<&'a str> {
    let found = string(value, key);
    ensure!(
        !found.is_empty(),
        "Missing native field {key} at source line {line}"
    );
    Ok(found)
}
fn timestamp(event: &Value) -> String {
    event
        .get("timestamp")
        .and_then(Value::as_str)
        .unwrap_or("1970-01-01T00:00:00Z")
        .into()
}

fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(values) => values
            .iter()
            .map(text)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => value.get("text").map(text).unwrap_or_default(),
        _ => String::new(),
    }
}

fn content_string(value: &Value) -> Result<String> {
    match value {
        Value::Null => Ok(String::new()),
        Value::String(text) => Ok(text.clone()),
        Value::Array(values)
            if values.iter().all(|block| {
                block.is_string()
                    || (matches!(string(block, "type"), "text" | "input_text" | "output_text")
                        && block.as_object().is_some_and(|map| {
                            map.keys()
                                .all(|key| matches!(key.as_str(), "type" | "text"))
                        }))
            }) =>
        {
            Ok(text(value))
        }
        _ => Ok(serde_json::to_string(value)?),
    }
}

fn identity(text: &str) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("{:x}", Sha256::digest(normalized.as_bytes()))
}

fn content_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(text) => text.trim().is_empty(),
        Value::Array(values) => values.iter().all(content_empty),
        Value::Object(map) => map.get("text").is_some_and(content_empty),
        _ => false,
    }
}

fn clean_content(value: &mut Value) {
    match value {
        Value::String(text) => *text = strip_injected(text),
        Value::Array(values) => {
            for value in values.iter_mut() {
                clean_content(value);
            }
            values.retain(|value| !content_empty(value));
        }
        Value::Object(map) => {
            if let Some(text) = map.get_mut("text") {
                clean_content(text);
            }
        }
        _ => {}
    }
}

const INJECTED_TAGS: &[&str] = &[
    "system-reminder",
    "user-prompt-submit-hook",
    "local-command-caveat",
    "local-command-stdout",
    "local-command-stderr",
    "command-stdout",
    "bash-stdout",
    "bash-stderr",
    "task-notification",
    "command-message",
    "codex_delegation",
    "realtime_delegation",
    "in-app-browser-context",
    "heartbeat",
    "system-note",
    "artifact-view-context",
    "environment_context",
    "user_instructions",
    "turn_aborted",
    "permissions",
    "skill",
    "hook",
    "subagent",
    "system",
    "collaboration_mode",
    "goal_state",
    "goal",
];

fn strip_injected(text: &str) -> String {
    let trimmed = text.trim_start();
    if [
        "Base directory for this skill:",
        "This session is being continued",
        "[Request interrupted",
        "# AGENTS.md",
        "You are an AI assistant",
        "<multi_agent",
        "Message Type: MESSAGE",
        "Message Type: FINAL_ANSWER",
    ]
    .iter()
    .any(|prefix| trimmed.starts_with(prefix))
    {
        return String::new();
    }
    let mut result = text.to_owned();
    for tag in INJECTED_TAGS {
        let opening = format!("<{tag}");
        let closing = format!("</{tag}>");
        let mut start = 0;
        while let Some(relative) = result[start..].find(&opening) {
            let index = start + relative;
            let boundary = index + opening.len();
            if !result[boundary..].starts_with('>')
                && !result[boundary..].starts_with(char::is_whitespace)
            {
                start = boundary;
                continue;
            }
            if let Some(end) = result[boundary..].find(&closing) {
                result.replace_range(index..boundary + end + closing.len(), "");
                start = index;
            } else if result[..index].trim().is_empty() {
                result.truncate(index);
                break;
            } else {
                start = boundary;
            }
        }
    }
    result.trim().to_owned()
}

fn native_call_extras(payload: &Value) -> Value {
    let mut extras = payload.clone();
    if let Some(fields) = extras.as_object_mut() {
        for field in ["call_id", "name", "arguments", "input"] {
            fields.remove(field);
        }
    }
    extras
}

fn encrypted_field_count(value: &Value) -> u64 {
    match value {
        Value::Object(fields) => {
            fields
                .iter()
                .map(|(key, value)| {
                    u64::from(matches!(
                        key.as_str(),
                        "encrypted_content" | "encrypted_function_args" | "encrypted_reasoning"
                    )) + encrypted_field_count(value)
                })
                .sum::<u64>()
                + u64::from(
                    string(value, "type") == "redacted_thinking" && fields.contains_key("data"),
                )
        }
        Value::Array(values) => values.iter().map(encrypted_field_count).sum(),
        _ => 0,
    }
}
