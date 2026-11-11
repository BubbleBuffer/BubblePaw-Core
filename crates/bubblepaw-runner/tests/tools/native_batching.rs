//! Native batches complete before the provider receives the next tool-result turn.
use anyhow::{Result, bail};
use async_trait::async_trait;
use bubblepaw_core::{
    ApprovalMode, ApprovalPolicy, CancellationToken, Harness, HarnessConfig, ModelRequest,
    ModelResponse, Provider, Role, RunOptions, Store, TextCallback, ToolCall, ToolRegistry,
};
use bubblepaw_runner::tools::register_native_tools;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

struct BatchProvider {
    calls: Vec<ToolCall>,
    requests: Mutex<Vec<ModelRequest>>,
}

#[async_trait]
impl Provider for BatchProvider {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let mut requests = self.requests.lock().unwrap();
        let round = requests.len();
        requests.push(request);
        match round {
            0 => Ok(ModelResponse {
                tool_calls: self.calls.clone(),
                ..Default::default()
            }),
            1 => Ok(ModelResponse {
                text: "Batch complete".into(),
                ..Default::default()
            }),
            _ => bail!("unexpected extra provider request"),
        }
    }
}

fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    }
}

async fn run_batch(root: &std::path::Path, calls: Vec<ToolCall>) -> Arc<BatchProvider> {
    let store = Store::in_memory().unwrap();
    let session = store.create_session("native batch").unwrap().id;
    let mut tools = ToolRegistry::new();
    let processes = register_native_tools(&mut tools, root, false).unwrap();
    let provider = Arc::new(BatchProvider {
        calls,
        requests: Mutex::default(),
    });
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        tools,
        HarnessConfig {
            approval: ApprovalPolicy {
                confirm: ApprovalMode::Allow,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let outcome = harness
        .prompt(&session, "Complete the batch", RunOptions::default())
        .await
        .unwrap();
    processes.shutdown().await;
    assert_eq!(outcome.status, "completed", "{:?}", outcome.error);
    assert!(store.pending_effects(&session).unwrap().is_empty());
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(!requests[0].tools.iter().any(|tool| tool.name == "js"));
        let messages = &requests[1].messages;
        let assistant = messages
            .iter()
            .rposition(|m| !m.tool_calls.is_empty())
            .unwrap();
        let results: Vec<_> = messages[assistant + 1..]
            .iter()
            .filter(|m| m.role == Role::Tool)
            .collect();
        assert_eq!(results.len(), provider.calls.len());
        for (result, call) in results.iter().zip(&provider.calls) {
            assert_eq!(result.tool_call_id.as_deref(), Some(call.id.as_str()));
            assert!(!result.is_error, "{}", result.content);
        }
    }
    provider
}

#[tokio::test]
async fn three_native_reads_return_together_in_one_tool_result_turn() {
    let root = tempfile::tempdir().unwrap();
    for name in ["one", "two", "three"] {
        std::fs::write(root.path().join(name), name).unwrap();
    }
    let provider = run_batch(
        root.path(),
        ["one", "two", "three"]
            .map(|name| call(name, "read", json!({"path":name})))
            .to_vec(),
    )
    .await;
    let requests = provider.requests.lock().unwrap();
    let results: Vec<_> = requests[1]
        .messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .collect();
    for (result, expected) in results.iter().zip(["one", "two", "three"]) {
        assert_eq!(
            serde_json::from_str::<Value>(&result.content).unwrap()["content"],
            expected
        );
    }
}

#[tokio::test]
async fn effectful_native_batch_runs_in_message_order() {
    let root = tempfile::tempdir().unwrap();
    let provider = run_batch(
        root.path(),
        vec![
            call(
                "write",
                "write",
                json!({"path":"ordered.txt","content":"before"}),
            ),
            call(
                "edit",
                "edit",
                json!({"path":"ordered.txt","old_string":"before","new_string":"after"}),
            ),
            call("read", "read", json!({"path":"ordered.txt"})),
        ],
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(root.path().join("ordered.txt")).unwrap(),
        "after"
    );
    let requests = provider.requests.lock().unwrap();
    let read = requests[1]
        .messages
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("read"))
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&read.content).unwrap()["content"],
        "after"
    );
}
