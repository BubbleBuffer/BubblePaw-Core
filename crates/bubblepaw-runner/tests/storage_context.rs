//! Synthetic local transport proof; private transcripts never enter a provider.
use anyhow::Result;
use axum::{Json, Router, body::Bytes, extract::State, routing::post};
use bubblepaw_core::{
    CacheTiers, CancellationToken, ContextCheckpoint, ContextStrategy, ContextView, Harness,
    HarnessConfig, Message, ModelRequest, Provider, Role, RunOptions, Store, StoreOptions,
    ToolRegistry,
};
use bubblepaw_runner::{
    provider::MessagesProvider,
    summarization::{SummarizingContext, SummaryConfig},
};
use serde_json::json;
use std::sync::{Arc, Mutex};

#[path = "common/hook.rs"]
mod hook;
use hook::prepare_fresh;

type Captured = Arc<Mutex<Vec<Vec<u8>>>>;
async fn server() -> Result<(MessagesProvider, Captured, tokio::task::JoinHandle<()>)> {
    let requests: Captured = Arc::default();
    let app = Router::new().route("/v1/messages", post(|State(requests): State<Captured>, bytes: Bytes| async move {
        requests.lock().unwrap().push(bytes.to_vec());
        Json(json!({"type":"message","role":"assistant","content":[{"type":"text","text":"Synthetic reply."}],
            "stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}))
    })).with_state(requests.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let provider = MessagesProvider::api_key(
        "synthetic-key",
        format!("http://{}/v1", listener.local_addr()?),
    )?;
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Ok((provider, requests, task))
}
fn message(session: &str, index: usize) -> Message {
    let content = format!(
        "Canonical synthetic message {index}: {}",
        "Exact spelling 猫 and newline\n".repeat(24)
    );
    let assistant = !index.is_multiple_of(2);
    Message {
        id: format!("signed-{index}"), session_id: session.into(),
        role: if assistant { Role::Assistant } else { Role::User },
        origin: None, content: content.clone(), tool_calls: Vec::new(), tool_call_id: None,
        provider_state: assistant.then(|| json!({"adapter":"anthropic-messages","content":[
            {"type":"thinking","thinking":"Synthetic signed reasoning.","signature":"signature+/==","opaque":{"keep":true}},
            {"type":"text","text":content}
        ],"stop_reason":"end_turn"})),
        is_error: false, created_at: "2026-10-12T00:00:00Z".into(),
    }
}
fn options() -> StoreOptions {
    StoreOptions {
        archive_after_days: 0,
        cold_min_messages: 16,
        cold_min_bytes: 1024,
        cold_tail_messages: 8,
        ..Default::default()
    }
}
fn request(messages: Vec<Message>) -> ModelRequest {
    ModelRequest {
        model: "claude-haiku-5-5".into(),
        system_prompt: "Synthetic frozen instructions.".into(),
        messages,
        ..Default::default()
    }
}
fn checkpoint(session: &str, through: &str) -> ContextCheckpoint {
    ContextCheckpoint {
        id: "synthetic-summary".into(),
        session_id: session.into(),
        through_message_id: through.into(),
        model: "synthetic-no-model".into(),
        summary: "Retained synthetic summary.".into(),
        files_touched: Vec::new(),
        created_at: "2026-10-12T00:00:00Z".into(),
        usage: None,
    }
}
async fn send(provider: &MessagesProvider, request: ModelRequest) -> Result<()> {
    provider
        .generate(request, Arc::new(|_| {}), CancellationToken::new())
        .await?;
    Ok(())
}

#[tokio::test]
async fn signed_http_request_bytes_match_after_dictionary_compression_and_archive() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_with_options(directory.path(), options())?;
    let session = store.create_session("synthetic wire")?;
    let messages: Vec<_> = (0..81).map(|index| message(&session.id, index)).collect();
    store.append_batch(&messages)?;
    store.save_context_checkpoint(&checkpoint(&session.id, &messages[69].id))?;
    let (provider, captured, server) = server().await?;
    send(&provider, request(store.messages(&session.id)?)).await?;
    assert!(store.compress_cold_messages(&session.id)? > 40);
    send(&provider, request(store.messages(&session.id)?)).await?;
    assert!(store.archive_chat(&session.id)?);
    send(&provider, request(store.messages(&session.id)?)).await?;
    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0], requests[1]);
    assert_eq!(requests[0], requests[2]);
    assert!(String::from_utf8_lossy(&requests[0]).contains("signature+/=="));
    server.abort();
    Ok(())
}

#[tokio::test]
async fn frozen_runtime_request_skips_unreadable_cold_rows_and_keeps_exact_wire_body() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let store = Store::open_with_options(directory.path(), options())?;
    let session =
        store.create_session_with_strategy("synthetic lazy", ContextStrategy::Classical)?;
    let messages: Vec<_> = (0..81).map(|index| message(&session.id, index)).collect();
    store.append_batch(&messages)?;
    let checkpoint = checkpoint(&session.id, &messages[69].id);
    store.save_context_checkpoint(&checkpoint)?;
    let mut summary = message(&session.id, 100);
    summary.id = "context:synthetic-summary".into();
    summary.role = Role::System;
    summary.content = checkpoint.summary.clone();
    summary.provider_state = None;
    let view = ContextView {
        session_id: session.id.clone(),
        frontier: Some(store.message_seq(&session.id, &messages[69].id)?),
        system_prompt: "Synthetic frozen instructions.".into(),
        prefix: vec![summary, messages[68].clone()],
        masked_tool_output_ids: Vec::new(),
        clipped_tool_call_ids: Vec::new(),
        created_at: "2026-10-12T00:00:00Z".into(),
        threshold: 256_000,
        budget: 128_000,
        tokens: 1000,
    };
    store.save_context_view(&view)?;
    let (provider, captured, server) = server().await?;
    let provider = Arc::new(provider);
    let context = Arc::new(SummarizingContext::new(
        store.clone(),
        provider.clone(),
        SummaryConfig::default(),
        None,
    )?);
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig {
            model: "claude-haiku-5-5".into(),
            system_prompt: "Synthetic frozen instructions.".into(),
            context: context.clone(),
            ..Default::default()
        },
    );
    // Compare identical chat prefixes, including the native discovery tools.
    let mut baseline_request = harness.request_template(&session.id)?;
    baseline_request.messages = store.messages(&session.id)?;
    let baseline =
        prepare_fresh(&*context, provider.clone(), &session.id, baseline_request).await?;
    let mut first = baseline.request;
    // The runtime path marks the end of the stored view prefix as the cold tier.
    first.cache_tiers = Some(CacheTiers {
        cold_end: Some(2),
        previous_len: None,
    });
    send(&provider, first).await?;
    assert!(store.compress_cold_messages(&session.id)? > 40);
    let path = store.chat_path(&session.id)?.unwrap();
    let connection = rusqlite::Connection::open(path)?;
    let encoded: String = connection.query_row(
        "SELECT data FROM messages WHERE message_id=?1",
        [&messages[0].id],
        |row| row.get(0),
    )?;
    let mut encoded: serde_json::Value = serde_json::from_str(&encoded)?;
    assert_eq!(encoded["_row_codec"], "zstd-dict-v1");
    encoded["data"] = json!("not-valid-base64!");
    connection.execute(
        "UPDATE messages SET data=?1 WHERE message_id=?2",
        rusqlite::params![encoded.to_string(), messages[0].id],
    )?;
    drop(connection);
    assert!(
        store.messages(&session.id).is_err(),
        "The cold-row poison must fail a full decode"
    );
    harness.start_resume(&session.id, RunOptions::default())?;
    assert_eq!(
        harness.wait_for_idle(&session.id).await?.status,
        "completed"
    );
    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
    server.abort();
    Ok(())
}
