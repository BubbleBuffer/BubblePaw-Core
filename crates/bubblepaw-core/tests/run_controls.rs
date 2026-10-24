mod support;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Capabilities, ContextPreparation, EventKind, Harness, HarnessConfig,
    JsonSchema, Message, MessageOrigin, ModelRequest, ModelResponse, PrepareCall, PreparedContext,
    Provider, Role, RunOptions, Store, TextCallback, Tool, ToolCall, ToolContext, ToolDefinition,
    ToolDispatcher, ToolRegistry, ToolResult, ToolSurface,
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
impl Scripted {
    fn new(responses: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(vec![]),
        })
    }
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
fn calls(name: &str, count: usize) -> ModelResponse {
    ModelResponse {
        tool_calls: (0..count)
            .map(|i| ToolCall {
                id: uuid::Uuid::new_v4().to_string(),
                name: name.into(),
                arguments: json!({"index":i}),
            })
            .collect(),
        ..Default::default()
    }
}
fn submit(value: Value) -> ModelResponse {
    ModelResponse {
        tool_calls: vec![ToolCall {
            id: uuid::Uuid::new_v4().to_string(),
            name: "submit_result".into(),
            arguments: value,
        }],
        ..Default::default()
    }
}
fn schema() -> JsonSchema {
    JsonSchema::new(
        "answer",
        json!({
            "type":"object", "additionalProperties":false,
            "properties":{"count":{"type":"integer", "minimum":1}}, "required":["count"]
        }),
    )
    .unwrap()
}
fn harness(provider: Arc<Scripted>, tools: ToolRegistry) -> Result<(Harness, Store, String)> {
    let store = Store::in_memory()?;
    let session = store.create_session("run controls")?.id;
    let harness = Harness::new(store.clone(), provider, tools, HarnessConfig::default());
    Ok((harness, store, session))
}

fn budget_notices(messages: &[Message]) -> Vec<&Message> {
    messages
        .iter()
        .filter(|message| {
            message.role == Role::User
                && message.origin == Some(MessageOrigin::Generated)
                && message.content.contains("<harness kind=\"tool_budget:")
        })
        .collect()
}

#[test]
fn disabled_tool_declarations_are_optional_and_round_trip() -> Result<()> {
    let legacy = json!({
        "model":"synthetic", "system_prompt":"stable", "messages":[],
        "tools":[], "max_output_tokens":null
    });
    let mut request: ModelRequest = serde_json::from_value(legacy)?;
    assert!(request.disabled_tools.is_empty());
    assert!(
        serde_json::to_value(&request)?
            .get("disabled_tools")
            .is_none()
    );
    request.disabled_tools = vec![Counter::default().definition()];
    let decoded: ModelRequest = serde_json::from_value(serde_json::to_value(&request)?)?;
    assert!(decoded.tools.is_empty());
    assert_eq!(
        serde_json::to_value(&decoded.disabled_tools)?,
        serde_json::to_value(&request.disabled_tools)?
    );
    Ok(())
}

struct Streamed(Arc<Scripted>);
#[async_trait]
impl Provider for Streamed {
    async fn generate(
        &self,
        request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let response = self
            .0
            .generate(request, on_text.clone(), cancellation)
            .await?;
        on_text(&response.text);
        Ok(response)
    }
}

#[path = "run_controls/structured.rs"]
mod structured;

#[derive(Default)]
struct Counter(AtomicUsize);
#[async_trait]
impl Tool for Counter {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "count".into(),
            description: "count invocations".into(),
            input_schema: json!({"type":"object"}),
            read_only: false,
            approval: Default::default(),
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(ToolResult::text(json!({"ok":true}).to_string()))
    }
}

#[tokio::test]
async fn batch_excess_is_denied_and_exhaustion_gets_final_request_at_round_limit() -> Result<()> {
    let counter = Arc::new(Counter::default());
    let mut tools = ToolRegistry::new();
    tools.register(counter.clone())?;
    let provider = Scripted::new(vec![calls("count", 4), reply("finished")]);
    let (harness, store, session) = harness(provider.clone(), tools)?;
    let run = harness
        .prompt(
            &session,
            "work",
            RunOptions {
                max_tool_calls: Some(2),
                max_rounds: Some(1),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(counter.0.load(Ordering::Relaxed), 2);
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests[0].system_prompt, requests[1].system_prompt);
    assert!(
        !requests[0]
            .system_prompt
            .contains("tool calls for this run")
    );
    assert!(
        budget_notices(&requests[0].messages)[0]
            .content
            .contains("You have 2 tool calls")
    );
    assert!(
        requests
            .iter()
            .all(|request| !budget_notices(&request.messages).is_empty())
    );
    assert!(requests[1].disabled_tools.is_empty());
    assert_eq!(
        serde_json::to_value(&requests[1].tools)?,
        serde_json::to_value(&requests[0].tools)?
    );
    assert_eq!(budget_notices(&store.messages(&session)?).len(), 2);
    let results: Vec<_> = store
        .messages(&session)?
        .into_iter()
        .filter(|m| m.role == Role::Tool)
        .collect();
    assert_eq!(results.len(), 4);
    assert!(results[0].content.ends_with("1 tool calls left"));
    assert!(results[1].content.ends_with("0 tool calls left"));
    assert!(
        results[2..]
            .iter()
            .all(|m| m.is_error && m.content.contains("not executed"))
    );
    Ok(())
}

#[tokio::test]
async fn zero_budget_refuses_an_ignored_reminder_then_enforces_tools_off() -> Result<()> {
    let counter = Arc::new(Counter::default());
    let mut tools = ToolRegistry::new();
    tools.register(counter.clone())?;
    let provider = Scripted::new(vec![calls("count", 1), reply("finished")]);
    let (harness, store, session) = harness(provider.clone(), tools)?;
    let run = harness
        .prompt(
            &session,
            "work",
            RunOptions {
                max_tool_calls: Some(0),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(counter.0.load(Ordering::Relaxed), 0);
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(!requests[0].tools.is_empty());
    assert!(requests[1].tools.is_empty());
    assert!(
        requests[1]
            .disabled_tools
            .iter()
            .any(|tool| tool.name == "count")
    );
    assert_eq!(budget_notices(&requests[0].messages).len(), 1);
    assert_eq!(budget_notices(&requests[1].messages).len(), 1);
    assert_eq!(requests[0].system_prompt, requests[1].system_prompt);
    assert_eq!(budget_notices(&store.messages(&session)?).len(), 1);
    assert!(
        store
            .messages(&session)?
            .iter()
            .any(|m| m.role == Role::Tool && m.is_error)
    );
    Ok(())
}

#[tokio::test]
async fn budget_notice_keeps_the_prefix_unchanged_through_active_rounds_and_exhaustion()
-> Result<()> {
    let counter = Arc::new(Counter::default());
    let mut tools = ToolRegistry::new();
    tools.register(counter.clone())?;
    let provider = Scripted::new(vec![
        calls("count", 1),
        calls("count", 1),
        reply("finished"),
    ]);
    let (harness, store, session) = harness(provider.clone(), tools)?;
    let run = harness
        .prompt(
            &session,
            "work",
            RunOptions {
                max_tool_calls: Some(2),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(counter.0.load(Ordering::Relaxed), 2);
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(!requests[0].tools.is_empty());
    assert!(!requests[1].tools.is_empty());
    assert!(!requests[2].tools.is_empty() && requests[2].disabled_tools.is_empty());
    let prefix = serde_json::to_value(&requests[0].messages)?;
    for request in requests.iter() {
        assert_eq!(request.system_prompt, requests[0].system_prompt);
        assert!(!request.system_prompt.contains("tool calls for this run"));
        assert!(!budget_notices(&request.messages).is_empty());
        assert_eq!(
            serde_json::to_value(&request.messages[..requests[0].messages.len()])?,
            prefix
        );
    }
    assert_eq!(
        serde_json::to_value(&requests[2].tools)?,
        serde_json::to_value(&requests[0].tools)?
    );
    assert_eq!(budget_notices(&store.messages(&session)?).len(), 2);
    Ok(())
}

#[path = "run_controls/js_result.rs"]
mod js_result;

#[tokio::test]
async fn budget_footer_must_fit_result_limit_before_input_is_admitted() -> Result<()> {
    let provider = Scripted::new(vec![]);
    let store = Store::in_memory()?;
    let session = store.create_session("tiny result limit")?.id;
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig {
            max_tool_result_bytes: 19,
            ..Default::default()
        },
    );
    // A two-digit budget needs 20 bytes even if the tool's actual output is empty.
    let error = harness
        .start(
            &session,
            "never admitted",
            RunOptions {
                max_tool_calls: Some(10),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(error.to_string().contains("at least 20 bytes"));
    assert!(store.messages(&session)?.is_empty());
    assert!(provider.requests.lock().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn every_new_run_has_a_fresh_explicit_budget_or_unlimited_default_and_full_history()
-> Result<()> {
    let counter = Arc::new(Counter::default());
    let mut tools = ToolRegistry::new();
    tools.register(counter.clone())?;
    let provider = Scripted::new(vec![
        calls("count", 1),
        reply("first"),
        calls("count", 1),
        reply("second"),
        calls("count", 3),
        reply("third"),
    ]);
    let (harness, store, session) = harness(provider.clone(), tools)?;
    for budget in [Some(1), Some(1), None] {
        let run = harness
            .prompt(
                &session,
                "next",
                RunOptions {
                    max_tool_calls: budget,
                    ..Default::default()
                },
            )
            .await?;
        assert_eq!(run.status, "completed", "{:?}", run.error);
    }
    assert_eq!(counter.0.load(Ordering::Relaxed), 5);
    let requests = provider.requests.lock().unwrap();
    assert!(requests[2].messages.iter().any(|m| m.content == "first"));
    assert!(requests[4].messages.iter().any(|m| m.content == "second"));
    assert_eq!(budget_notices(&store.messages(&session)?).len(), 4);
    assert_eq!(budget_notices(&requests[0].messages).len(), 1);
    assert_eq!(budget_notices(&requests[2].messages).len(), 3);
    assert_eq!(budget_notices(&requests[4].messages).len(), 4);
    assert!(
        budget_notices(&requests[2].messages)
            .last()
            .unwrap()
            .content
            .contains("1 tool calls left")
    );
    assert!(
        budget_notices(&requests[4].messages)
            .last()
            .unwrap()
            .content
            .contains("0 of 1 tool calls left")
    );
    assert!(
        !requests[4]
            .system_prompt
            .contains("tool calls for this run")
    );
    let last_results: Vec<_> = store
        .messages(&session)?
        .into_iter()
        .rev()
        .filter(|m| m.role == Role::Tool)
        .take(3)
        .collect();
    assert!(
        last_results
            .iter()
            .all(|m| !m.content.contains("tool calls left"))
    );
    Ok(())
}

#[path = "run_controls/replacement.rs"]
mod replacement;

#[path = "run_controls/composition.rs"]
mod composition;
