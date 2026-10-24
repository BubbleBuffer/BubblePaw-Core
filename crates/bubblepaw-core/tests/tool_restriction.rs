//! A run's `tool_names` restricts calls at call time: declarations and the system
//! prompt stay those of the session profile, and the restriction is stated once.
mod support;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Observation,
    ObservationMode, ObservationRecord, ObservationSink, Provider, Role, RunOptions, Store,
    TextCallback, Tool, ToolCall, ToolContext, ToolDefinition, ToolRegistry, ToolResult,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use support::reply;

struct Scripted {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
}

#[async_trait]
impl Provider for Scripted {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
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

struct Named {
    name: &'static str,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for Named {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.into(),
            description: format!("{} tool", self.name),
            input_schema: json!({"type":"object"}),
            read_only: true,
            approval: Default::default(),
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(ToolResult::text(json!({"ok":true}).to_string()))
    }
}

#[derive(Default)]
struct Sink(Mutex<Vec<ObservationRecord>>);

impl ObservationSink for Sink {
    fn record(&self, record: ObservationRecord) {
        self.0.lock().unwrap().push(record);
    }
}

fn call(id: &str, name: &str) -> ModelResponse {
    ModelResponse {
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: json!({}),
        }],
        ..Default::default()
    }
}

#[tokio::test]
async fn restricted_run_keeps_declarations_and_refuses_disallowed_calls() -> Result<()> {
    let reads = Arc::new(AtomicUsize::new(0));
    let writes = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Named {
        name: "read",
        calls: reads.clone(),
    }))?;
    tools.register(Arc::new(Named {
        name: "write",
        calls: writes.clone(),
    }))?;
    let provider = Arc::new(Scripted {
        responses: Mutex::new(
            vec![
                call("unrestricted-read", "read"),
                reply("unrestricted done"),
                call("restricted-write", "write"),
                reply("restricted done"),
            ]
            .into(),
        ),
        requests: Mutex::default(),
    });
    let sink = Arc::new(Sink::default());
    let store = Store::in_memory()?;
    let session = store.create_session("tool restriction")?;
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        tools,
        HarnessConfig {
            observation: Some(Observation {
                sink: sink.clone(),
                mode: ObservationMode::Metadata,
                max_payload_bytes: 1024,
            }),
            ..Default::default()
        },
    );

    let unrestricted = harness
        .prompt(&session.id, "first", RunOptions::default())
        .await?;
    assert_eq!(unrestricted.status, "completed");
    let restricted = harness
        .prompt(
            &session.id,
            "second",
            RunOptions {
                tool_names: Some(vec!["read".into()]),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(restricted.status, "completed");

    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    let declarations = serde_json::to_value(&requests[0].tools)?;
    assert_eq!(
        requests[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["call_tool", "read", "tool_search", "write"]),
    );
    for request in requests.iter() {
        assert_eq!(serde_json::to_value(&request.tools)?, declarations);
        assert_eq!(request.system_prompt, requests[0].system_prompt);
    }

    let notice = "<harness kind=\"tool_restriction:";
    let notices = |request: &ModelRequest| {
        request
            .messages
            .iter()
            .filter(|message| message.content.contains(notice))
            .count()
    };
    assert_eq!(notices(&requests[0]), 0);
    assert_eq!(notices(&requests[1]), 0);
    // The notice is queued once; the second request sees it in history, not a second copy.
    assert_eq!(notices(&requests[2]), 1);
    assert_eq!(notices(&requests[3]), 1);

    let rejection = requests[3]
        .messages
        .iter()
        .find(|message| message.role == Role::Tool && message.is_error)
        .expect("restricted run returns the refused call as an error result");
    assert!(
        rejection
            .content
            .contains("Tool `write` is not available in this run. Allowed tools: read."),
        "{}",
        rejection.content
    );
    assert_eq!(writes.load(Ordering::Relaxed), 0);
    assert_eq!(reads.load(Ordering::Relaxed), 1);

    let records = sink.0.lock().unwrap();
    let classes: Vec<&str> = records
        .iter()
        .filter(|record| record.kind == "context_built")
        .filter_map(|record| record.metrics["classification"]["kind"].as_str())
        .collect();
    assert_eq!(classes.len(), 4);
    assert_eq!(classes[0], "first");
    assert!(classes.iter().all(|kind| *kind != "frozen_change"));
    Ok(())
}
