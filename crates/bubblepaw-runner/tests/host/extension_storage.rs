//! Synthetic extension RPC -> authoritative SQLite artifact lifecycle audit.
use anyhow::{Result, ensure};
use bubblepaw_core::{
    CancellationToken, ContextCheckpoint, Message, ModelRequest, Provider, Role, Store,
    StoreOptions,
};
use bubblepaw_runner::provider::{ExtensionConfig, ExtensionProvider};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::sync::Arc;

const DATE: &str = "2026-10-13T00:00:00Z";

fn options() -> StoreOptions {
    StoreOptions {
        archive_after_days: 0,
        cold_min_messages: u32::MAX,
        cold_min_bytes: u64::MAX,
        ..Default::default()
    }
}

fn message(session: &str, index: usize) -> Message {
    Message {
        id: format!("extension-artifact-{index}"),
        session_id: session.into(),
        role: if index.is_multiple_of(2) {
            Role::User
        } else {
            Role::Assistant
        },
        origin: None,
        content: "Synthetic history 猫 π 🐾\n".repeat(240),
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: DATE.into(),
    }
}

fn exported(bytes: &[u8]) -> Result<Vec<Message>> {
    std::str::from_utf8(bytes)?
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

fn assert_exact(
    messages: &[Message],
    baseline: &[u8],
    signature: &str,
    redacted: &str,
) -> Result<()> {
    ensure!(messages.len() == 41, "message count changed");
    let state = messages[1].provider_state.as_ref().unwrap();
    ensure!(
        serde_json::to_vec(state)? == baseline,
        "serialized extension provider_state bytes changed"
    );
    ensure!(
        state["content"][0]["signature"]
            .as_str()
            .unwrap()
            .as_bytes()
            == signature.as_bytes(),
        "signature bytes changed"
    );
    ensure!(
        state["content"][1]["data"].as_str().unwrap().as_bytes() == redacted.as_bytes(),
        "redacted artifact bytes changed"
    );
    Ok(())
}

#[tokio::test]
async fn extension_opaque_bytes_survive_sqlite_reopen_cold_archive_export_and_reimport()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let signature = "signature+/==雪\nπ🐾".repeat(900);
    let redacted = "opaque+/==猫\n\r\tπ".repeat(900);
    let native = json!({"adapter":"anthropic-messages",
    "_bubblepaw_origin":{"provider_kind":"anthropic-messages","model":"direct-model","source":"native"},
    "content":[
        {"type":"thinking","thinking":"Plan 猫\nπ 🐾","signature":signature,"unknown":{"future":[true,null,7],"signed_zero":-0.0}},
        {"type":"redacted_thinking","data":redacted,"opaque":{"keep":"雪"}}
    ]});
    // Expected bytes come from the synthetic source, before extension transport.
    // SDK objects have no original HTTP wire spelling available at this boundary.
    let baseline = serde_json::to_vec(&native)?;
    let fixture_state = directory.path().join("provider-state.json");
    std::fs::write(&fixture_state, &baseline)?;
    let provider = ExtensionProvider::new(ExtensionConfig {
        id: "fake".into(),
        command: env!("CARGO_BIN_EXE_bubblepaw-fake-extension").into(),
        args: vec![format!("--provider-state-file={}", fixture_state.display())],
        cwd: Some(directory.path().into()),
        ..Default::default()
    });
    let mut store = Store::open_with_options(directory.path().join("data"), options())?;
    let session = store.create_session("synthetic extension opaque audit")?;
    let mut messages: Vec<_> = (0..41).map(|index| message(&session.id, index)).collect();
    // Keep the assistant row large after opaque state moves into its blob, so
    // the artifact's owning row itself exercises dictionary compression.
    let mut prompt = messages[0].clone();
    prompt.content = format!("text {}", messages[1].content);
    let response = provider
        .generate(
            ModelRequest {
                model: "direct-model".into(),
                session_id: session.id.clone(),
                messages: vec![prompt],
                ..Default::default()
            },
            Arc::new(|_| {}),
            CancellationToken::new(),
        )
        .await?;
    messages[1].content = response.text;
    messages[1].tool_calls = response.tool_calls;
    messages[1].provider_state = response.provider_state;
    assert_exact(&messages, &baseline, &signature, &redacted)?;
    store.append_batch(&messages)?;
    assert_exact(
        &store.messages(&session.id)?,
        &baseline,
        &signature,
        &redacted,
    )?;
    let path = store.chat_path(&session.id)?.unwrap();
    {
        let connection = Connection::open(&path)?;
        let raw: String = connection.query_row(
            "SELECT data FROM messages WHERE message_id=?1",
            [&messages[1].id],
            |row| row.get(0),
        )?;
        let row: Value = serde_json::from_str(&raw)?;
        ensure!(
            row["_provider_state_blob"].is_string() && row["provider_state"].is_null(),
            "large opaque state must have one blob owner"
        );
    }
    store.idle_checkpoint()?;
    drop(store);
    store = Store::open_with_options(directory.path().join("data"), options())?;
    assert_exact(
        &store.messages(&session.id)?,
        &baseline,
        &signature,
        &redacted,
    )?;
    store.save_context_checkpoint(&ContextCheckpoint {
        id: "extension-artifact-checkpoint".into(),
        session_id: session.id.clone(),
        through_message_id: messages[37].id.clone(),
        model: "synthetic-no-model".into(),
        summary: "Synthetic checkpoint.".into(),
        files_touched: vec![],
        created_at: DATE.into(),
        usage: None,
    })?;
    ensure!(
        store.compress_cold_messages_with_options(
            &session.id,
            StoreOptions {
                cold_tail_messages: 0,
                cold_min_messages: 0,
                cold_min_bytes: 0,
                ..options()
            }
        )? > 0,
        "no rows compressed"
    );
    {
        let connection = Connection::open(&path)?;
        let raw: String = connection.query_row(
            "SELECT data FROM messages WHERE message_id=?1",
            [&messages[1].id],
            |row| row.get(0),
        )?;
        ensure!(
            serde_json::from_str::<Value>(&raw)?["_row_codec"] == "zstd-dict-v1",
            "artifact row did not exercise dictionary compression"
        );
    }
    assert_exact(
        &store.messages(&session.id)?,
        &baseline,
        &signature,
        &redacted,
    )?;
    let mut ordinary_export = Vec::new();
    store.export_jsonl(&session.id, &mut ordinary_export)?;
    assert_exact(
        &exported(&ordinary_export)?,
        &baseline,
        &signature,
        &redacted,
    )?;
    ensure!(store.archive_chat(&session.id)?, "archive did not run");
    let archive = path.with_extension("db.zst");
    ensure!(
        !path.exists() && archive.exists(),
        "archive placement missing"
    );
    let mut archive_export = Vec::new();
    Store::export_data_dir_jsonl(
        directory.path().join("data"),
        &session.id,
        &mut archive_export,
    )?;
    ensure!(
        !path.exists() && archive.exists(),
        "read-only export changed archive placement"
    );
    let mut imported_messages = exported(&archive_export)?;
    assert_exact(&imported_messages, &baseline, &signature, &redacted)?;
    assert_exact(
        &store.messages(&session.id)?,
        &baseline,
        &signature,
        &redacted,
    )?;
    ensure!(
        path.exists() && !archive.exists(),
        "access did not unarchive"
    );
    let target = tempfile::tempdir()?;
    let imported = Store::open_with_options(target.path(), options())?;
    let imported_session = imported.create_session("synthetic canonical JSONL reimport")?;
    for message in &mut imported_messages {
        message.session_id.clone_from(&imported_session.id);
    }
    imported.append_batch(&imported_messages)?;
    assert_exact(
        &imported.messages(&imported_session.id)?,
        &baseline,
        &signature,
        &redacted,
    )?;
    provider.close_session(&session.id).await?;
    Ok(())
}
