//! `details` (for interfaces, never sent to a model) and `terminate` (ends the run) on tool results.
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, Role,
    RunOptions, Store, StreamCallback, Tool, ToolCall, ToolContext, ToolDefinition, ToolRegistry,
    ToolResult,
};
use serde_json::{Value, json};
use std::{collections::VecDeque, sync::Arc, sync::Mutex};

struct Scripted {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
}
#[async_trait]
impl Provider for Scripted {
    async fn generate(
        &self,
        request: ModelRequest,
        _: StreamCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow!("Unexpected request"))
    }
}

/// `report` returns details; `finish` also terminates; `ping` is plain.
struct Probe(&'static str);
#[async_trait]
impl Tool for Probe {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.0.into(),
            description: "probe".into(),
            input_schema: json!({"type":"object"}),
            read_only: false,
            approval: Default::default(),
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        let result = ToolResult::text("shown to the model");
        Ok(match self.0 {
            "report" => result.with_details(json!({"rows":3})),
            "finish" => result.with_details(json!({"done":true})).with_terminate(),
            _ => result,
        })
    }
}

fn call(name: &str) -> ToolCall {
    ToolCall {
        id: format!("call-{name}"),
        name: name.into(),
        arguments: json!({}),
    }
}

async fn run(
    responses: Vec<ModelResponse>,
    tools: &[&'static str],
) -> Result<(Arc<Scripted>, Store, String)> {
    let provider = Arc::new(Scripted {
        responses: Mutex::new(responses.into()),
        requests: Mutex::new(vec![]),
    });
    let mut registry = ToolRegistry::new();
    for name in tools {
        registry.register(Arc::new(Probe(name)))?;
    }
    let store = Store::in_memory()?;
    let session = store.create_session("details")?.id;
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        registry,
        HarnessConfig::default(),
    );
    let run = harness
        .prompt(&session, "work", RunOptions::default())
        .await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    Ok((provider, store, session))
}

#[tokio::test]
async fn details_are_stored_with_the_message_and_never_reach_the_model() -> Result<()> {
    let calls = ModelResponse {
        tool_calls: vec![call("report")],
        ..Default::default()
    };
    let done = ModelResponse {
        text: "done".into(),
        ..Default::default()
    };
    let (provider, store, session) = run(vec![calls, done], &["report"]).await?;
    let stored = store
        .messages(&session)?
        .into_iter()
        .find(|message| message.role == Role::Tool)
        .unwrap();
    assert_eq!(stored.text(), "shown to the model");
    assert_eq!(serde_json::to_value(&stored)?["details"], json!({"rows":3}));
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .messages
            .iter()
            .filter(|message| message.role == Role::Tool)
            .all(|message| message.details.is_none() && message.text() == "shown to the model")
    );
    Ok(())
}

#[tokio::test]
async fn a_terminating_result_ends_the_run_after_its_whole_batch() -> Result<()> {
    let batch = ModelResponse {
        tool_calls: vec![call("finish"), call("ping")],
        ..Default::default()
    };
    // No second response is queued: a further model request would fail the run.
    let (provider, store, session) = run(vec![batch], &["finish", "ping"]).await?;
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    let results: Vec<_> = store
        .messages(&session)?
        .into_iter()
        .filter(|message| message.role == Role::Tool)
        .collect();
    assert_eq!(results.len(), 2);
    Ok(())
}
