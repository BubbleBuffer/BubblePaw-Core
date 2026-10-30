#![allow(dead_code)]
use anyhow::{Context, Result};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, Message, ModelRequest, ModelResponse, Provider,
    Role, Store, TextCallback, ToolRegistry,
};
use rusqlite::{Connection, params};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

pub fn sql_chat_path(path: &Path, session: &str) -> Result<PathBuf> {
    Store::open(path)?
        .chat_path(session)?
        .context("Chat path missing")
}
pub fn sql_serving_path(path: &Path) -> PathBuf {
    Store::legacy_data_dir(path).join("serving.db")
}

/// Materialize a genuine old schema fixture from the split store. These files
/// belong to a test TempDir; original user databases are never replaced.
pub fn replace_with_legacy_fixture(path: &Path, version: i64) -> Result<()> {
    let schema = include_str!("../fixtures/main-v6-schema.sql");
    let next = match version {
        1 => Some("ALTER TABLE effects ADD COLUMN detail"),
        2 => Some("CREATE TABLE context_checkpoints"),
        3 => Some("CREATE TABLE context_chapters"),
        4 => Some("ALTER TABLE inputs ADD COLUMN origin"),
        5 => Some("CREATE TABLE context_views"),
        6 => None,
        _ => anyhow::bail!("Unsupported fixture version"),
    };
    let schema = next.map_or(schema, |marker| schema.split_once(marker).unwrap().0);
    let store = Store::open(path)?;
    let sessions = store.list_sessions()?;
    let mut messages = HashMap::new();
    let mut runs = HashMap::new();
    let mut checkpoints = HashMap::new();
    let mut views = HashMap::new();
    let mut inputs = HashMap::new();
    let mut derived: HashMap<&str, HashMap<String, String>> = HashMap::new();
    let mut paths = HashSet::new();
    for session in &sessions {
        paths.insert(
            store
                .chat_path(&session.id)?
                .context("Fixture chat missing")?,
        );
        for message in store.messages(&session.id)? {
            messages.insert(message.id.clone(), serde_json::to_string(&message)?);
        }
        for run in store.runs(&session.id)? {
            runs.insert(run.id.clone(), serde_json::to_string(&run)?);
        }
        for input in store.queued_inputs(&session.id)? {
            inputs.insert(input.id.clone(), input.text);
        }
        if let Some(checkpoint) = store.context_checkpoint(&session.id)? {
            checkpoints.insert(session.id.clone(), serde_json::to_string(&checkpoint)?);
        }
        if let Some(view) = store.context_view(&session.id)? {
            views.insert(session.id.clone(), serde_json::to_string(&view)?);
        }
        for (table, records) in [
            (
                "context_chapters",
                store
                    .context_chapters(&session.id)?
                    .into_iter()
                    .map(|record| Ok((record.id.clone(), serde_json::to_string(&record)?)))
                    .collect::<Result<Vec<_>>>()?,
            ),
            (
                "context_episodes",
                store
                    .context_episodes(&session.id)?
                    .into_iter()
                    .map(|record| Ok((record.id.clone(), serde_json::to_string(&record)?)))
                    .collect::<Result<Vec<_>>>()?,
            ),
            (
                "context_subject_chapters",
                store
                    .context_subject_chapters(&session.id)?
                    .into_iter()
                    .map(|record| Ok((record.id.clone(), serde_json::to_string(&record)?)))
                    .collect::<Result<Vec<_>>>()?,
            ),
            (
                "context_decisions",
                store
                    .context_decisions(&session.id)?
                    .into_iter()
                    .map(|record| Ok((record.id.clone(), serde_json::to_string(&record)?)))
                    .collect::<Result<Vec<_>>>()?,
            ),
        ] {
            derived.entry(table).or_default().extend(records);
        }
    }
    store.idle_checkpoint()?;
    let serving_path = sql_serving_path(path);
    drop(store);
    let legacy_path = path.with_extension("legacy-fixture.sqlite");
    let conn = Connection::open(&legacy_path)?;
    conn.execute_batch(schema)?;
    let present = |table: &str| -> Result<bool> {
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |row| row.get(0),
        )?)
    };
    let mut offset = 0;
    for chat_path in paths {
        let chat = Connection::open(&chat_path)?;
        let maximum: i64 =
            chat.query_row("SELECT COALESCE(MAX(seq),0) FROM messages", [], |row| {
                row.get(0)
            })?;
        for table in [
            "metadata",
            "sessions",
            "messages",
            "runs",
            "inputs",
            "effects",
            "outputs",
            "context_checkpoints",
            "context_chapters",
            "context_subject_chapters",
            "context_episodes",
            "context_decisions",
        ] {
            if !present(table)? {
                continue;
            }
            let columns = conn
                .prepare(&format!("PRAGMA table_info({table})"))?
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut statement =
                chat.prepare(&format!("SELECT {} FROM {table}", columns.join(",")))?;
            let records = statement
                .query_map([], |row| {
                    (0..columns.len())
                        .map(|column| row.get::<_, rusqlite::types::Value>(column))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for mut values in records {
                let text = |field: &str| -> Option<String> {
                    columns
                        .iter()
                        .position(|column| column == field)
                        .and_then(|column| match &values[column] {
                            rusqlite::types::Value::Text(value) => Some(value.clone()),
                            _ => None,
                        })
                };
                let key = text("id");
                let session = text("session_id");
                let data = match table {
                    "messages" => {
                        let raw: serde_json::Value = serde_json::from_str(&text("data").unwrap())?;
                        messages.get(raw["id"].as_str().unwrap()).cloned()
                    }
                    "runs" => key.as_ref().and_then(|key| runs.get(key)).cloned(),
                    "context_checkpoints" => session
                        .as_ref()
                        .and_then(|key| checkpoints.get(key))
                        .cloned(),
                    _ => key
                        .as_ref()
                        .and_then(|key| derived.get(table).and_then(|records| records.get(key)))
                        .cloned(),
                };
                if let Some(data) = data {
                    values[columns.iter().position(|column| column == "data").unwrap()] =
                        data.into();
                }
                if table == "inputs"
                    && let Some(input) = key.as_ref().and_then(|key| inputs.get(key))
                {
                    values[columns.iter().position(|column| column == "text").unwrap()] =
                        input.clone().into();
                }
                for (index, column) in columns.iter().enumerate() {
                    if ([
                        "first_message_seq",
                        "last_message_seq",
                        "through_message_seq",
                    ]
                    .contains(&column.as_str())
                        || (table == "messages" && column == "seq"))
                        && let rusqlite::types::Value::Integer(value) = &mut values[index]
                    {
                        *value += offset;
                    }
                    if (table == "inputs" || table == "outputs") && column == "seq" {
                        values[index] = rusqlite::types::Value::Null;
                    }
                }
                let sql = format!(
                    "INSERT OR REPLACE INTO {table}({}) VALUES({})",
                    columns.join(","),
                    columns.iter().map(|_| "?").collect::<Vec<_>>().join(",")
                );
                conn.execute(&sql, rusqlite::params_from_iter(values))?;
            }
        }
        offset += maximum;
    }
    // Finished operations and views are read from their disposable projection.
    let serving = Connection::open(&serving_path)?;
    for table in ["runs", "context_views"] {
        if !present(table)? {
            continue;
        }
        if table == "context_views" {
            for (session, data) in &views {
                conn.execute(
                    "INSERT OR REPLACE INTO context_views VALUES(?1,?2)",
                    params![session, data],
                )?;
            }
        } else {
            let mut statement = serving.prepare("SELECT id,session_id,status FROM runs")?;
            for row in statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })? {
                let (key, session, status) = row?;
                if let Some(data) = runs.get(&key) {
                    conn.execute(
                        "INSERT OR REPLACE INTO runs VALUES(?1,?2,?3,?4)",
                        params![key, session, status, data],
                    )?;
                }
            }
        }
    }
    drop(serving);
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?,
        version
    );
    drop(conn);
    let data_dir = Store::legacy_data_dir(path);
    std::fs::remove_dir_all(data_dir)?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    std::fs::rename(legacy_path, path)?;
    Ok(())
}

/// A provider that answers every request with "answer" and never calls a model.
struct FixedProvider;

#[async_trait]
impl Provider for FixedProvider {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(ModelResponse {
            text: "answer".into(),
            ..Default::default()
        })
    }
}

/// A harness over `store` whose provider is `FixedProvider`.
pub fn harness(store: &Store) -> Harness {
    Harness::new(
        store.clone(),
        Arc::new(FixedProvider),
        ToolRegistry::new(),
        HarnessConfig::default(),
    )
}

/// A model reply that carries `text` and no tool calls.
pub fn reply(text: &str) -> ModelResponse {
    ModelResponse {
        text: text.into(),
        ..Default::default()
    }
}

/// A stored message with the storage tests' fixed timestamp.
pub fn stored_message(session: &str, id: &str, role: Role, content: &str) -> Message {
    Message {
        id: id.into(),
        session_id: session.into(),
        role,
        origin: None,
        content: content.into(),
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: "2026-10-12T00:00:00Z".into(),
    }
}

/// A stored user message with the storage tests' fixed timestamp.
pub fn stored_text(session: &str, id: &str, content: &str) -> Message {
    stored_message(session, id, Role::User, content)
}

/// A message whose timestamp is a placeholder, for tests that never read it.
pub fn unstamped_message(session: &str, id: &str, role: Role, content: &str) -> Message {
    Message {
        id: id.into(),
        session_id: session.into(),
        role,
        origin: None,
        content: content.into(),
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: "time".into(),
    }
}

/// A message with a fresh id.
pub fn uuid_message(session: &str, role: Role, text: &str) -> Message {
    Message {
        id: uuid::Uuid::new_v4().to_string(),
        session_id: session.into(),
        role,
        origin: None,
        content: text.into(),
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: "2026-10-12T12:00:00Z".into(),
    }
}
