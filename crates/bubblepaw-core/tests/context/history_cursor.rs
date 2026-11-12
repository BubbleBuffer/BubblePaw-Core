//! The harness computes the history cursor centrally, after context preparation.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ContextPreparation, Harness, HarnessConfig, HistoryCursor, Message,
    ModelRequest, ModelResponse, Provider, Role, RunOptions, Store, TextCallback, ToolCall,
    ToolContext, ToolDefinition, ToolRegistry, ToolResult,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Recording(Mutex<Vec<ModelRequest>>);
#[async_trait]
impl Provider for Recording {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let call = request
            .messages
            .last()
            .filter(|m| m.role == Role::User && m.content == "use a tool")
            .map(|_| ToolCall {
                id: uuid::Uuid::new_v4().to_string(),
                name: "noop".into(),
                arguments: json!({}),
            });
        self.0.lock().unwrap().push(request);
        Ok(ModelResponse {
            text: "answer".into(),
            tool_calls: call.into_iter().collect(),
            ..Default::default()
        })
    }
}
struct Noop;
#[async_trait]
impl bubblepaw_core::Tool for Noop {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            approval: Default::default(),
            name: "noop".into(),
            description: "noop".into(),
            input_schema: json!({"type":"object"}),
            read_only: true,
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        Ok(ToolResult::text("done"))
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Full,
    /// Replace everything after the first message by one summary message.
    SummarizeTail,
    /// Replace the first message.
    ReplaceFirst,
}
struct Strategy(Mutex<Mode>, Mutex<Option<Message>>);
impl ContextPreparation for Strategy {
    fn project_history(&self, history: &[Message]) -> Result<Vec<Message>> {
        let mode = *self.0.lock().unwrap();
        let summary = || {
            self.1
                .lock()
                .unwrap()
                .get_or_insert_with(|| Message {
                    id: uuid::Uuid::new_v4().to_string(),
                    session_id: history[0].session_id.clone(),
                    role: Role::User,
                    origin: None,
                    content: "summary".into(),
                    tool_calls: vec![],
                    tool_call_id: None,
                    provider_state: None,
                    is_error: false,
                    created_at: "2026-10-08T00:00:00Z".into(),
                })
                .clone()
        };
        Ok(match mode {
            Mode::Full => history.to_vec(),
            Mode::SummarizeTail => {
                let mut messages = vec![history[0].clone(), summary()];
                messages.push(history.last().unwrap().clone());
                messages
            }
            Mode::ReplaceFirst => {
                let mut messages = history.to_vec();
                messages[0] = summary();
                messages
            }
        })
    }
}

#[tokio::test]
async fn cursor_tracks_appends_and_rewrites_after_preparation() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("cursor")?.id;
    let provider = Arc::new(Recording::default());
    let strategy = Arc::new(Strategy(Mutex::new(Mode::Full), Mutex::new(None)));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Noop))?;
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        tools,
        HarnessConfig {
            context: strategy.clone(),
            ..HarnessConfig::default()
        },
    );
    let cursors = |provider: &Recording| -> Vec<(String, HistoryCursor)> {
        provider
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|r| (r.session_id.clone(), r.history.clone()))
            .collect()
    };
    // First step: new epoch, nothing rewritten. A tool round appends.
    harness
        .prompt(&session, "use a tool", RunOptions::default())
        .await?;
    let steps = cursors(&provider);
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].0, session);
    assert!(!steps[0].1.epoch.is_empty());
    assert_eq!(steps[0].1.rewritten_from, None);
    assert_eq!(steps[1].1, steps[0].1);
    let epoch = steps[0].1.epoch.clone();
    // A new prompt appends again.
    harness
        .prompt(&session, "plain", RunOptions::default())
        .await?;
    assert_eq!(cursors(&provider)[2].1.epoch, epoch);
    // A strategy that replaces an earlier part of the prefix.
    *strategy.0.lock().unwrap() = Mode::SummarizeTail;
    harness
        .prompt(&session, "after summary", RunOptions::default())
        .await?;
    let rewritten = cursors(&provider)[3].1.clone();
    let summary_id = strategy.1.lock().unwrap().as_ref().unwrap().id.clone();
    assert_ne!(rewritten.epoch, epoch);
    assert_eq!(
        rewritten.rewritten_from.as_deref(),
        Some(summary_id.as_str())
    );
    // A different first message rebuilds from the start.
    *strategy.0.lock().unwrap() = Mode::ReplaceFirst;
    harness
        .prompt(&session, "first replaced", RunOptions::default())
        .await?;
    let front = cursors(&provider)[4].1.clone();
    assert_ne!(front.epoch, rewritten.epoch);
    assert_eq!(front.rewritten_from, None);
    // Another harness has no memory: a fresh epoch.
    let other = Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    other
        .prompt(&session, "restart", RunOptions::default())
        .await?;
    let restarted = cursors(&provider)[5].1.clone();
    assert_ne!(restarted.epoch, front.epoch);
    assert_eq!(restarted.rewritten_from, None);
    Ok(())
}
