//! Independent native-wire audit with synthetic opaque data and loopback HTTP only.
use anyhow::{Result, ensure};
use axum::{Json, Router, body::Bytes, extract::State, response::IntoResponse, routing::post};
use bubblepaw_core::{
    CancellationToken, ContextCheckpoint, Message, ModelRequest, Provider, ReasoningReplay, Role,
    Session, Store, StoreOptions,
};
use bubblepaw_runner::provider::{ChatCompletionsProvider, MessagesProvider, ResponsesProvider};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
};

const DATE: &str = "2026-10-13T00:00:00Z";
type Captured = Arc<Mutex<Vec<Vec<u8>>>>;

#[derive(Clone, Copy)]
enum Kind {
    Responses,
    Messages,
    Chat,
}
impl Kind {
    fn adapter(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::Messages => "anthropic-messages",
            Self::Chat => "chat-completions",
        }
    }
    fn model(self) -> &'static str {
        match self {
            Self::Responses => "gpt-6-sol",
            Self::Messages => "claude-haiku-5-5",
            Self::Chat => "synthetic-chat-reasoner",
        }
    }
    fn other_model(self) -> &'static str {
        match self {
            Self::Responses => "gpt-6-luna",
            Self::Messages => "claude-sonnet-5-5",
            Self::Chat => "other-chat-reasoner",
        }
    }
    fn field(self) -> &'static str {
        match self {
            Self::Responses => "items",
            Self::Messages => "content",
            Self::Chat => "message_fields",
        }
    }
    fn other(self) -> Self {
        match self {
            Self::Responses => Self::Messages,
            Self::Messages => Self::Responses,
            Self::Chat => Self::Responses,
        }
    }
}

fn digest(value: &Value) -> Result<Vec<u8>> {
    Ok(Sha256::digest(serde_json::to_vec(value)?).to_vec())
}
fn raw_digest(value: &[u8]) -> Vec<u8> {
    Sha256::digest(value).to_vec()
}
fn native_blocks(kind: Kind) -> Value {
    let text = "Visible canonical α🐾\n".repeat(240);
    match kind {
        Kind::Responses => json!([
            {"type":"reasoning","id":"reasoning-opaque-id","encrypted_content":"cipher+/==猫\n".repeat(900),
             "summary":[{"type":"summary_text","text":"Synthetic plan π","opaque":{"keep":true}}],
             "unknown":{"unicode":"雪🐾","signed_zero":-0.0,"future":[1,null,true]}},
            {"type":"message","id":"native-message-id","role":"assistant","status":"completed",
             "content":[{"type":"output_text","text":text,"annotations":[{"unknown":"猫"}]}]},
            {"type":"reasoning","id":"second-reasoning-id","encrypted_content":"tail+/==π", "summary":[],"future":12}
        ]),
        Kind::Messages => json!([
            {"type":"thinking","thinking":"Synthetic plan 猫\nπ🐾", "signature":"signature+/==雪\n".repeat(900),
             "unknown":{"signed_zero":-0.0,"future":[1,null,true]}},
            {"type":"text","text":text,"opaque":{"keep":"猫"}},
            {"type":"redacted_thinking","data":"redacted+/==猫\n".repeat(900),"unknown":[true,{"π":"🐾"}]},
            {"type":"thinking","thinking":"After text","signature":"tail-signature+/==", "future":12}
        ]),
        Kind::Chat => json!({
            "reasoning_content":"chat-reasoning+/==猫\nπ🐾".repeat(900),
            "reasoning":"OpenRouter plan 猫\nπ🐾",
            "reasoning_details":[
                {"type":"reasoning.text","text":"OpenRouter plan 猫\nπ🐾","signature":null,"id":null,"format":"unknown","index":0},
                {"type":"reasoning.encrypted","data":"cipher+/==猫\n".repeat(900),"id":"r1","format":"openai-responses-v1","index":1}
            ],
            "unknown":{"unicode":"雪🐾","signed_zero":-0.0,"future":[1,null,true]}
        }),
    }
}

struct Fixture {
    provider: Arc<dyn Provider>,
    captured: Captured,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn fixture(kind: Kind) -> Result<Fixture> {
    let captured: Captured = Arc::default();
    let response = match kind {
        Kind::Responses => {
            json!({"type":"response.completed","response":{"output":native_blocks(kind),"usage":{"input_tokens":1,"output_tokens":1}}})
        }
        Kind::Messages => {
            json!({"type":"message","role":"assistant","content":native_blocks(kind),"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}})
        }
        Kind::Chat => {
            let mut message = native_blocks(kind);
            message["role"] = json!("assistant");
            message["content"] = json!("Visible canonical α🐾\n".repeat(240));
            json!({"choices":[{"message":message,"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}})
        }
    };
    let route = match kind {
        Kind::Responses => "/responses",
        Kind::Messages => "/v1/messages",
        Kind::Chat => "/chat/completions",
    };
    let app = Router::new()
        .route(
            route,
            post(move |State(captured): State<Captured>, body: Bytes| {
                let response = response.clone();
                async move {
                    captured.lock().unwrap().push(body.to_vec());
                    match kind {
                        Kind::Responses => (
                            [("content-type", "text/event-stream")],
                            format!("data: {response}\r\n\r\n"),
                        )
                            .into_response(),
                        Kind::Messages | Kind::Chat => Json(response).into_response(),
                    }
                }
            }),
        )
        .with_state(captured.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let provider: Arc<dyn Provider> = match kind {
        Kind::Responses => Arc::new(ResponsesProvider::api_key("synthetic-key", base)?),
        Kind::Messages => Arc::new(MessagesProvider::api_key(
            "synthetic-key",
            format!("{base}/v1"),
        )?),
        Kind::Chat => Arc::new(
            ChatCompletionsProvider::api_key("synthetic-key", base)?.with_reasoning_replay(
                BTreeMap::from([
                    (kind.model().into(), ReasoningReplay::All),
                    (kind.other_model().into(), ReasoningReplay::All),
                ]),
            )?,
        ),
    };
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Ok(Fixture {
        provider,
        captured,
        task,
    })
}
fn request(kind: Kind, messages: Vec<Message>) -> ModelRequest {
    ModelRequest {
        model: kind.model().into(),
        system_prompt: "Synthetic fixed instructions.".into(),
        session_id: "synthetic-wire-session".into(),
        messages,
        max_output_tokens: Some(100),
        ..Default::default()
    }
}
async fn send(fixture: &Fixture, request: ModelRequest) -> Result<Vec<u8>> {
    fixture
        .provider
        .generate(request, Arc::new(|_| {}), CancellationToken::new())
        .await?;
    Ok(fixture.captured.lock().unwrap().last().unwrap().clone())
}
fn message(session: &str, index: usize) -> Message {
    Message {
        id: format!("synthetic-message-{index}"),
        session_id: session.into(),
        role: if index.is_multiple_of(2) {
            Role::User
        } else {
            Role::Assistant
        },
        origin: None,
        content: format!(
            "Canonical source {index}: {}",
            "Exact spelling π 猫 🐾\n".repeat(240)
        ),
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: DATE.into(),
    }
}
async fn history(fixture: &Fixture, kind: Kind, session: &str) -> Result<Vec<Message>> {
    let response = fixture
        .provider
        .generate(
            request(kind, vec![message(session, 0)]),
            Arc::new(|_| {}),
            CancellationToken::new(),
        )
        .await?;
    let mut messages: Vec<_> = (0..41).map(|index| message(session, index)).collect();
    messages[1].content = response.text;
    messages[1].tool_calls = response.tool_calls;
    messages[1].provider_state = response.provider_state;
    let state = messages[1].provider_state.as_ref().unwrap();
    ensure!(
        state["_bubblepaw_origin"]["provider_kind"] == kind.adapter(),
        "native origin missing"
    );
    ensure!(
        state["_bubblepaw_origin"]["model"] == kind.model(),
        "native model missing"
    );
    // An independent native source assertion catches capture loss before the baseline.
    ensure!(
        digest(&opaque(kind, &state[kind.field()]))?
            == digest(&opaque(kind, &native_blocks(kind)))?,
        "capture changed opaque data"
    );
    Ok(messages)
}
fn opaque(kind: Kind, blocks: &Value) -> Value {
    if matches!(kind, Kind::Chat) {
        return if blocks.get("reasoning_content").is_some() {
            json!([blocks])
        } else {
            json!([])
        };
    }
    json!(
        blocks
            .as_array()
            .unwrap()
            .iter()
            .filter(|block| match kind {
                Kind::Responses => block["type"] == "reasoning",
                Kind::Messages =>
                    block["type"] == "thinking" || block["type"] == "redacted_thinking",
                Kind::Chat => unreachable!(),
            })
            .cloned()
            .collect::<Vec<_>>()
    )
}
fn opaque_wire(kind: Kind, bytes: &[u8]) -> Result<Value> {
    let body: Value = serde_json::from_slice(bytes)?;
    Ok(match kind {
        Kind::Responses => opaque(kind, &body["input"]),
        Kind::Messages => json!(
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|message| message["content"].as_array().into_iter().flatten())
                .filter(|block| block["type"] == "thinking" || block["type"] == "redacted_thinking")
                .cloned()
                .collect::<Vec<_>>()
        ),
        Kind::Chat => json!(
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| message.get("reasoning_content").is_some())
                .map(|message| {
                    let mut fields = message.as_object().unwrap().clone();
                    fields.remove("role");
                    fields.remove("content");
                    Value::Object(fields)
                })
                .collect::<Vec<_>>()
        ),
    })
}
fn options() -> StoreOptions {
    StoreOptions {
        archive_after_days: 0,
        cold_min_messages: u32::MAX,
        cold_min_bytes: u64::MAX,
        ..Default::default()
    }
}
fn exported(bytes: &[u8]) -> Result<Vec<Message>> {
    String::from_utf8(bytes.to_vec())?
        .lines()
        .filter_map(|line| {
            let value: Result<Value, _> = serde_json::from_str(line);
            match value {
                Ok(value) => value
                    .get("message")
                    .map(|message| serde_json::from_value(message.clone()).map_err(Into::into)),
                Err(error) => Some(Err(error.into())),
            }
        })
        .collect()
}
fn compress(store: &Store, session: &str, messages: &[Message]) -> Result<()> {
    store.save_context_checkpoint(&ContextCheckpoint {
        id: "synthetic-checkpoint".into(),
        session_id: session.into(),
        through_message_id: messages[37].id.clone(),
        model: "synthetic-no-model".into(),
        summary: "Synthetic durable summary.".into(),
        files_touched: vec![],
        created_at: DATE.into(),
        usage: None,
    })?;
    ensure!(
        store.compress_cold_messages_with_options(
            session,
            StoreOptions {
                cold_tail_messages: 0,
                cold_min_messages: 0,
                cold_min_bytes: 0,
                ..options()
            }
        )? > 0,
        "no row compressed"
    );
    let conn = Connection::open(store.chat_path(session)?.unwrap())?;
    let raw: String = conn.query_row(
        "SELECT data FROM messages WHERE message_id=?1",
        [&messages[1].id],
        |row| row.get(0),
    )?;
    ensure!(
        serde_json::from_str::<Value>(&raw)?["_row_codec"] == "zstd-dict-v1",
        "signed row must exercise dictionary codec"
    );
    Ok(())
}

#[derive(Clone, Copy)]
enum Pathway {
    Store,
    Cold,
    Archive,
    Unarchive,
    Reopen,
    Export,
    Reimport,
    PhaseB,
    Legacy,
}
async fn audit(kind: Kind, pathway: Pathway) -> Result<()> {
    let dir = tempfile::tempdir()?;
    let fixture = fixture(kind).await?;
    let mut store = Store::open_with_options(dir.path(), options())?;
    let session = store.create_session("synthetic opaque audit")?;
    let mut original = history(&fixture, kind, &session.id).await?;
    if matches!(pathway, Pathway::Legacy | Pathway::PhaseB) {
        // Frozen full native originless state, as persisted by old adapters.
        original[1].provider_state =
            Some(json!({"adapter":kind.adapter(), kind.field():native_blocks(kind)}));
    }
    let before_state = digest(original[1].provider_state.as_ref().unwrap())?;
    // This request is generated from the in-memory source before any append.
    let baseline = send(&fixture, request(kind, original.clone())).await?;
    ensure!(
        digest(&opaque_wire(kind, &baseline)?)? == digest(&opaque(kind, &native_blocks(kind)))?,
        "baseline must contain exact native opaque blocks"
    );
    store.append_batch(&original)?;
    if !matches!(pathway, Pathway::Legacy | Pathway::PhaseB) {
        let conn = Connection::open(store.chat_path(&session.id)?.unwrap())?;
        let raw: String = conn.query_row(
            "SELECT data FROM messages WHERE message_id=?1",
            [&original[1].id],
            |row| row.get(0),
        )?;
        let raw: Value = serde_json::from_str(&raw)?;
        ensure!(
            raw["_provider_state_blob"].is_string(),
            ">4KiB provider state must exercise its blob reference"
        );
        ensure!(
            raw["provider_state"].is_null(),
            "opaque body must not be stored twice in row"
        );
    }
    let after = match pathway {
        Pathway::Store => store.messages(&session.id)?,
        Pathway::Cold => {
            compress(&store, &session.id, &original)?;
            store.messages(&session.id)?
        }
        Pathway::Archive => {
            compress(&store, &session.id, &original)?;
            let path = store.chat_path(&session.id)?.unwrap();
            ensure!(store.archive_chat(&session.id)?, "archive did not run");
            let archived = path.with_extension("db.zst");
            ensure!(
                !path.exists() && archived.exists(),
                "archive placement missing"
            );
            let mut bytes = Vec::new();
            Store::export_data_dir_jsonl(dir.path(), &session.id, &mut bytes)?;
            ensure!(
                !path.exists() && archived.exists(),
                "read-only archive export must leave archive intact"
            );
            exported(&bytes)?
        }
        Pathway::Unarchive => {
            compress(&store, &session.id, &original)?;
            let path = store.chat_path(&session.id)?.unwrap();
            ensure!(store.archive_chat(&session.id)?, "archive did not run");
            ensure!(!path.exists(), "chat must begin archived");
            let messages = store.messages(&session.id)?;
            ensure!(
                path.exists() && !path.with_extension("db.zst").exists(),
                "access must unarchive chat"
            );
            messages
        }
        Pathway::Reopen => {
            compress(&store, &session.id, &original)?;
            store.idle_checkpoint()?;
            drop(store);
            store = Store::open_with_options(dir.path(), options())?;
            store.messages(&session.id)?
        }
        Pathway::Export | Pathway::Reimport => {
            compress(&store, &session.id, &original)?;
            let mut bytes = Vec::new();
            store.export_jsonl(&session.id, &mut bytes)?;
            let mut messages = exported(&bytes)?;
            if matches!(pathway, Pathway::Reimport) {
                let target = tempfile::tempdir()?;
                let imported = Store::open_with_options(target.path(), options())?;
                let session = imported.create_session("canonical JSONL Message import")?;
                for message in &mut messages {
                    message.session_id.clone_from(&session.id);
                }
                imported.append_batch(&messages)?;
                messages = imported.messages(&session.id)?;
            }
            messages
        }
        Pathway::PhaseB => {
            let path = store.chat_path(&session.id)?.unwrap();
            store.idle_checkpoint()?;
            drop(store);
            let conn = Connection::open(&path)?;
            // Remove only C additions; restore a real B row's full originless JSON.
            for message in &original {
                conn.execute(
                    "UPDATE messages SET data=?1 WHERE message_id=?2",
                    params![serde_json::to_string(message)?, message.id],
                )?;
            }
            let bodies: Vec<String> = conn
                .prepare("SELECT data FROM messages ORDER BY seq")?
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            conn.execute_batch("DROP TABLE message_tool_calls; DROP TABLE message_headers; DROP TABLE message_dictionaries; PRAGMA user_version=9; PRAGMA wal_checkpoint(TRUNCATE);")?;
            drop(conn);
            store = Store::open_with_options(dir.path(), options())?;
            let messages = store.messages(&session.id)?;
            let conn = Connection::open(&path)?;
            let restored: Vec<String> = conn
                .prepare("SELECT data FROM messages ORDER BY seq")?
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            ensure!(
                bodies == restored,
                "B upgrade must not rewrite original rows"
            );
            messages
        }
        Pathway::Legacy => {
            let legacy_dir = tempfile::tempdir()?;
            let source = legacy_dir.path().join("legacy-v6.sqlite");
            legacy_fixture(&source, &session, &original)?;
            let before = raw_digest(&std::fs::read(&source)?);
            let target = legacy_dir.path().join("migrated");
            Store::migrate_legacy(&source, &target)?;
            ensure!(
                before == raw_digest(&std::fs::read(&source)?),
                "legacy source bytes changed"
            );
            Store::open_with_options(&target, options())?.messages(&session.id)?
        }
    };
    ensure!(
        before_state == digest(after[1].provider_state.as_ref().unwrap())?,
        "opaque canonical state changed"
    );
    ensure!(original.len() == after.len(), "message count changed");
    for (before, after) in original.iter().zip(&after) {
        let mut before = serde_json::to_value(before)?;
        let mut after = serde_json::to_value(after)?;
        // Canonical Message import intentionally assigns the destination session.
        before.as_object_mut().unwrap().remove("session_id");
        after.as_object_mut().unwrap().remove("session_id");
        ensure!(
            digest(&before)? == digest(&after)?,
            "message payload hash changed"
        );
    }
    let restored = send(&fixture, request(kind, after)).await?;
    ensure!(
        raw_digest(&baseline) == raw_digest(&restored) && baseline == restored,
        "next native HTTP body differs from pre-storage baseline"
    );
    Ok(())
}
fn legacy_fixture(path: &Path, session: &Session, messages: &[Message]) -> Result<()> {
    let conn = Connection::open(path)?;
    conn.execute_batch(include_str!(
        "../../bubblepaw-core/tests/fixtures/main-v6-schema.sql"
    ))?;
    conn.execute(
        "INSERT INTO sessions(id,data) VALUES(?1,?2)",
        params![session.id, serde_json::to_string(session)?],
    )?;
    for message in messages {
        conn.execute(
            "INSERT INTO messages(session_id,data) VALUES(?1,?2)",
            params![session.id, serde_json::to_string_pretty(message)?],
        )?;
    }
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")?;
    Ok(())
}

macro_rules! path_test {
    ($name:ident, $kind:ident, $path:ident) => {
        #[tokio::test]
        async fn $name() -> Result<()> {
            audit(Kind::$kind, Pathway::$path).await
        }
    };
}
path_test!(
    responses_store_keeps_opaque_and_next_native_bytes,
    Responses,
    Store
);
path_test!(
    messages_store_keeps_opaque_and_next_native_bytes,
    Messages,
    Store
);
path_test!(
    responses_dictionary_keeps_opaque_and_next_native_bytes,
    Responses,
    Cold
);
path_test!(
    messages_dictionary_keeps_opaque_and_next_native_bytes,
    Messages,
    Cold
);
path_test!(
    responses_archive_keeps_opaque_and_next_native_bytes,
    Responses,
    Archive
);
path_test!(
    messages_archive_keeps_opaque_and_next_native_bytes,
    Messages,
    Archive
);
path_test!(
    responses_unarchive_keeps_opaque_and_next_native_bytes,
    Responses,
    Unarchive
);
path_test!(
    messages_unarchive_keeps_opaque_and_next_native_bytes,
    Messages,
    Unarchive
);
path_test!(
    responses_reopen_keeps_opaque_and_next_native_bytes,
    Responses,
    Reopen
);
path_test!(
    messages_reopen_keeps_opaque_and_next_native_bytes,
    Messages,
    Reopen
);
path_test!(
    responses_export_keeps_opaque_and_next_native_bytes,
    Responses,
    Export
);
path_test!(
    messages_export_keeps_opaque_and_next_native_bytes,
    Messages,
    Export
);
path_test!(
    responses_canonical_reimport_keeps_opaque_and_next_native_bytes,
    Responses,
    Reimport
);
path_test!(
    messages_canonical_reimport_keeps_opaque_and_next_native_bytes,
    Messages,
    Reimport
);
path_test!(
    responses_phase_b_upgrade_keeps_originless_state_and_native_bytes,
    Responses,
    PhaseB
);
path_test!(
    messages_phase_b_upgrade_keeps_originless_state_and_native_bytes,
    Messages,
    PhaseB
);
path_test!(
    responses_legacy_migration_keeps_originless_state_and_native_bytes,
    Responses,
    Legacy
);
path_test!(
    messages_legacy_migration_keeps_originless_state_and_native_bytes,
    Messages,
    Legacy
);

path_test!(chat_store_keeps_opaque_and_next_native_bytes, Chat, Store);
path_test!(
    chat_dictionary_keeps_opaque_and_next_native_bytes,
    Chat,
    Cold
);
path_test!(
    chat_archive_keeps_opaque_and_next_native_bytes,
    Chat,
    Archive
);
path_test!(
    chat_unarchive_keeps_opaque_and_next_native_bytes,
    Chat,
    Unarchive
);
path_test!(chat_reopen_keeps_opaque_and_next_native_bytes, Chat, Reopen);
path_test!(chat_export_keeps_opaque_and_next_native_bytes, Chat, Export);
path_test!(
    chat_canonical_reimport_keeps_opaque_and_next_native_bytes,
    Chat,
    Reimport
);
path_test!(
    chat_phase_b_upgrade_keeps_originless_state_and_native_bytes,
    Chat,
    PhaseB
);
path_test!(
    chat_legacy_migration_keeps_originless_state_and_native_bytes,
    Chat,
    Legacy
);

async fn incompatible(kind: Kind, foreign_kind: bool) -> Result<()> {
    let source = fixture(kind).await?;
    let original = history(&source, kind, "synthetic-filter").await?;
    let target_kind = if foreign_kind { kind.other() } else { kind };
    let target = fixture(target_kind).await?;
    let before = digest(&serde_json::to_value(&original)?)?;
    let mut request = request(target_kind, original.clone());
    if !foreign_kind {
        request.model = kind.other_model().into();
    }
    let bytes = send(&target, request).await?;
    ensure!(
        opaque_wire(target_kind, &bytes)?
            .as_array()
            .unwrap()
            .is_empty(),
        "foreign opaque blocks reached wire"
    );
    let body = String::from_utf8(bytes)?;
    ensure!(
        ![
            "signature+/==",
            "cipher+/==",
            "redacted+/==",
            "chat-reasoning+/=="
        ]
        .iter()
        .any(|marker| body.contains(marker)),
        "foreign opaque fields reached wire"
    );
    ensure!(
        before == digest(&serde_json::to_value(original)?)?,
        "filter changed canonical data"
    );
    Ok(())
}
#[tokio::test]
async fn responses_to_messages_omits_foreign_opaque_state() -> Result<()> {
    incompatible(Kind::Responses, true).await
}
#[tokio::test]
async fn messages_to_responses_omits_foreign_opaque_state() -> Result<()> {
    incompatible(Kind::Messages, true).await
}
#[tokio::test]
async fn responses_known_model_mismatch_omits_opaque_state() -> Result<()> {
    incompatible(Kind::Responses, false).await
}
#[tokio::test]
async fn messages_known_model_mismatch_omits_opaque_state() -> Result<()> {
    incompatible(Kind::Messages, false).await
}
#[tokio::test]
async fn chat_to_responses_omits_foreign_opaque_state() -> Result<()> {
    incompatible(Kind::Chat, true).await
}
#[tokio::test]
async fn chat_known_model_mismatch_omits_opaque_state() -> Result<()> {
    incompatible(Kind::Chat, false).await
}
