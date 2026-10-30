#![allow(dead_code)]
use anyhow::{Context, Result};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, Message, ModelRequest, ModelResponse, Provider,
    Role, Store, TextCallback, ToolRegistry,
};
use std::{
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
