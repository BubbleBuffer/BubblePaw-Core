use anyhow::{Result, anyhow};
use async_trait::async_trait;
use bubblepaw_core::*;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

#[derive(Default)]
struct Scripted {
    replies: Mutex<VecDeque<ModelResponse>>,
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
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow!("Unexpected request"))
    }
}

fn call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.into(),
        arguments,
    }
}
fn batch(calls: Vec<ToolCall>) -> ModelResponse {
    ModelResponse {
        tool_calls: calls,
        ..Default::default()
    }
}
fn submit(value: Value) -> ModelResponse {
    batch(vec![call("submit_result", value)])
}
fn text(value: &str) -> ModelResponse {
    ModelResponse {
        text: value.into(),
        ..Default::default()
    }
}
fn options() -> RunOptions {
    RunOptions {
        output_schema: Some(
            JsonSchema::new(
                "answer",
                json!({
                    "type":"object", "properties":{"answer":{"type":"integer"}},
                    "required":["answer"], "additionalProperties":false
                }),
            )
            .unwrap(),
        ),
        ..Default::default()
    }
}

#[derive(Default)]
struct Effect(AtomicUsize);
#[async_trait]
impl Tool for Effect {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "effect".into(),
            description: "fixture".into(),
            input_schema: json!({"type":"object"}),
            approval: Approval::None,
            read_only: false,
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::text("done"))
    }
}

struct Fixture {
    harness: Harness,
    store: Store,
    session: String,
    provider: Arc<Scripted>,
    effect: Arc<Effect>,
}
fn fixture(replies: Vec<ModelResponse>) -> Result<Fixture> {
    let store = Store::in_memory()?;
    let session = store.create_session("submit result")?.id;
    let provider = Arc::new(Scripted {
        replies: Mutex::new(replies.into()),
        ..Default::default()
    });
    let effect = Arc::new(Effect::default());
    let mut tools = ToolRegistry::new();
    tools.register_host(effect.clone())?;
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        tools,
        HarnessConfig::default(),
    );
    Ok(Fixture {
        harness,
        store,
        session,
        provider,
        effect,
    })
}

#[tokio::test]
async fn native_result_is_declared_from_first_request_with_exact_schema_and_finishes_immediately()
-> Result<()> {
    let f = fixture(vec![submit(json!({"answer":7}))])?;
    let expected = f
        .harness
        .effective_tool_definitions(&f.session, &options())?;
    let run = f.harness.prompt(&f.session, "compute", options()).await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(run.text.as_deref(), Some(r#"{"answer":7}"#));
    let requests = f.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        serde_json::to_value(&requests[0].tools)?,
        serde_json::to_value(expected)?
    );
    let result = requests[0].tools.last().unwrap();
    assert_eq!(result.name, "submit_result");
    assert_eq!(result.input_schema, options().output_schema.unwrap().schema);
    assert!(requests[0].output_schema.is_none());
    let history = f.store.messages(&f.session)?;
    let final_message = history.last().unwrap();
    assert_eq!(final_message.content, run.text.unwrap());
    assert_eq!(final_message.origin, Some(MessageOrigin::Generated));
    assert!(
        history
            .iter()
            .any(|message| message.role == Role::Tool && !message.is_error)
    );
    Ok(())
}

#[tokio::test]
async fn invalid_submission_is_a_tool_error_and_valid_retry_finishes() -> Result<()> {
    let f = fixture(vec![
        submit(json!({"answer":"bad"})),
        submit(json!({"answer":9})),
    ])?;
    let run = f.harness.prompt(&f.session, "compute", options()).await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(run.text.as_deref(), Some(r#"{"answer":9}"#));
    let requests = f.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].messages.iter().any(|message| {
        message.role == Role::Tool
            && message.is_error
            && message
                .content
                .contains("Invalid arguments for submit_result")
    }));
    assert_eq!(
        serde_json::to_value(&requests[0].tools)?,
        serde_json::to_value(&requests[1].tools)?
    );
    assert!(
        requests
            .iter()
            .all(|request| request.output_schema.is_none())
    );
    Ok(())
}

#[tokio::test]
async fn call_tool_can_submit_despite_an_empty_run_allowlist() -> Result<()> {
    let f = fixture(vec![batch(vec![call(
        "call_tool",
        json!({"name":"submit_result","arguments":{"answer":4}}),
    )])])?;
    let mut options = options();
    options.tool_names = Some(vec![]);
    let run = f.harness.prompt(&f.session, "compute", options).await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(run.text.as_deref(), Some(r#"{"answer":4}"#));
    Ok(())
}

#[tokio::test]
async fn native_result_runs_after_other_calls_and_first_submission_wins() -> Result<()> {
    let f = fixture(vec![batch(vec![
        call("submit_result", json!({"answer":1})),
        call("effect", json!({})),
        call("submit_result", json!({"answer":2})),
    ])])?;
    let run = f.harness.prompt(&f.session, "compute", options()).await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(run.text.as_deref(), Some(r#"{"answer":1}"#));
    assert_eq!(f.effect.0.load(Ordering::SeqCst), 1);
    let tools: Vec<_> = f
        .store
        .messages(&f.session)?
        .into_iter()
        .filter(|message| message.role == Role::Tool)
        .collect();
    assert_eq!(tools.len(), 3);
    assert_eq!(tools[0].content, "done");
    assert!(!tools[1].is_error);
    assert!(tools[2].is_error && tools[2].content.contains("already accepted"));
    Ok(())
}

#[tokio::test]
async fn nudge_is_total_per_run_and_unrelated_calls_do_not_reset_it() -> Result<()> {
    let f = fixture(vec![
        text("plain"),
        batch(vec![call("effect", json!({}))]),
        text("again"),
    ])?;
    let run = f.harness.prompt(&f.session, "compute", options()).await?;
    assert_eq!(run.status, "failed");
    assert!(
        run.error
            .unwrap()
            .contains("submit_result was not called after one host reminder")
    );
    assert_eq!(f.provider.requests.lock().unwrap().len(), 3);
    assert_eq!(f.effect.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.store
            .messages(&f.session)?
            .iter()
            .filter(|message| message.content.contains("<harness kind=\"result_required:"))
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn host_lcm_completion_tools_stay_unknown_to_main_call_tool() -> Result<()> {
    let f = fixture(vec![
        batch(vec![
            call("call_tool", json!({"name":"submit_summary","arguments":{}})),
            call("call_tool", json!({"name":"submit_chapter","arguments":{}})),
        ]),
        submit(json!({"answer":3})),
    ])?;
    let run = f.harness.prompt(&f.session, "compute", options()).await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(
        f.store
            .messages(&f.session)?
            .iter()
            .filter(|message| message.role == Role::Tool
                && message.is_error
                && message.content.contains("Unknown tool"))
            .count(),
        2
    );
    Ok(())
}

#[path = "submit_result/controls.rs"]
mod controls;
#[path = "submit_result/schema_changes.rs"]
mod schema_changes;
#[path = "submit_result/schema_transitions.rs"]
mod schema_transitions;
