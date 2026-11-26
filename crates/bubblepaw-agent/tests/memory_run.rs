//! Real runs of the loop over the in-memory seams and a scripted provider.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_agent::live_events::LiveEvent;
use bubblepaw_agent::loop_options::LoopOptions;
use bubblepaw_agent::memory::{JournalEvent, MemoryHost};
use bubblepaw_agent::tool_gate::{GateCall, ToolGate, Verdict};
use bubblepaw_agent::{Tool, ToolContext, ToolRegistry, ToolResult};
use bubblepaw_ai::adapter::{ScriptedProvider, ScriptedStep};
use bubblepaw_ai::{
    CancellationToken, Message, ModelRequest, ModelResponse, Provider, Role, StreamCallback,
    ToolCall, ToolDefinition,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// Records the inputs it is called with and answers `echo: <text>`.
#[derive(Default)]
struct Echo {
    inputs: Mutex<Vec<Value>>,
}

#[async_trait]
impl Tool for Echo {
    fn definition(&self) -> ToolDefinition {
        let schema = json!({
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
            "additionalProperties": false
        });
        ToolDefinition::new("echo", "Echo the text back.", schema)
    }
    async fn invoke(&self, _context: ToolContext, input: Value) -> Result<ToolResult> {
        let text = input["text"].as_str().unwrap_or_default().to_owned();
        self.inputs.lock().unwrap().push(input);
        Ok(ToolResult::text(format!("echo: {text}")))
    }
}

/// A scripted provider that keeps the requests it saw and can cancel the run
/// while the model call is in flight.
struct Watched {
    inner: ScriptedProvider,
    requests: Mutex<Vec<ModelRequest>>,
    cancel_in_call: Option<CancellationToken>,
}

#[async_trait]
impl Provider for Watched {
    async fn generate(
        &self,
        request: ModelRequest,
        on_text: StreamCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request.clone());
        if let Some(token) = &self.cancel_in_call {
            token.cancel();
        }
        self.inner.generate(request, on_text, cancellation).await
    }
}

/// Refuses every call, so `before` decides each one.
struct DenyAll;

#[async_trait]
impl ToolGate for DenyAll {
    fn admits_without_write(&self, _: &GateCall<'_>, _: &ToolDefinition) -> Result<bool> {
        Ok(false)
    }
    async fn before(&self, _: &GateCall<'_>, _: &ToolDefinition) -> Result<Verdict> {
        Ok(Verdict::Deny("denied by test gate".into()))
    }
    fn after(&self, _: &GateCall<'_>, result: ToolResult) -> Result<ToolResult> {
        Ok(result)
    }
}

fn echo_calls(count: usize) -> Vec<ScriptedStep> {
    (0..count)
        .map(|n| ScriptedStep::Calls {
            calls: vec![ToolCall {
                id: format!("call-{n}"),
                name: "echo".into(),
                arguments: json!({"text": "hi"}),
            }],
        })
        .collect()
}

struct Fixture {
    host: MemoryHost,
    tool: Arc<Echo>,
    provider: Arc<Watched>,
}

fn fixture(steps: Vec<ScriptedStep>, cancel_in_call: Option<CancellationToken>) -> Fixture {
    let tool = Arc::new(Echo::default());
    let mut registry = ToolRegistry::new();
    registry.register(tool.clone()).unwrap();
    let provider = Arc::new(Watched {
        inner: ScriptedProvider::with_steps(steps, "done"),
        requests: Mutex::default(),
        cancel_in_call,
    });
    let host = MemoryHost::new("scripted", provider.clone(), registry, "Be brief.");
    Fixture {
        host,
        tool,
        provider,
    }
}

fn texts(messages: &[Message]) -> Vec<(Role, String)> {
    let text = |m: &Message| (m.role.clone(), m.text().into_owned());
    messages.iter().map(text).collect()
}

#[tokio::test]
async fn one_reply_finishes_the_run_and_is_journalled() {
    let steps = vec![ScriptedStep::Text {
        text: "hello there".into(),
    }];
    let f = fixture(steps, None);
    let token = CancellationToken::new();
    let outcome = f.host.run("hi", &LoopOptions::default(), &token).await;
    assert_eq!(outcome.status, "completed");
    assert_eq!(outcome.text.as_deref(), Some("hello there"));
    assert_eq!(
        texts(&f.host.journal.transcript()),
        [
            (Role::User, "hi".into()),
            (Role::Assistant, "hello there".into())
        ]
    );
}

#[tokio::test]
async fn a_tool_call_runs_and_its_result_reaches_the_next_request() {
    let f = fixture(echo_calls(1), None);
    let token = CancellationToken::new();
    let outcome = f.host.run("go", &LoopOptions::default(), &token).await;
    assert_eq!(outcome.status, "completed");
    assert_eq!(outcome.text.as_deref(), Some("done"));
    assert_eq!(*f.tool.inputs.lock().unwrap(), [json!({"text": "hi"})]);
    // The result is journalled, between the call and the final reply.
    let results = f.host.journal.tool_results();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].tool_call_id.as_deref(), Some("call-0"));
    assert_eq!(results[0].text(), "echo: hi");
    let transcript = f.host.journal.transcript();
    let roles: Vec<_> = transcript.iter().map(|m| m.role.clone()).collect();
    assert_eq!(
        roles,
        [Role::User, Role::Assistant, Role::Tool, Role::Assistant]
    );
    // The second request carries the call and its result.
    let requests = f.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let sent = &requests[1].messages;
    assert_eq!(sent[1].tool_calls[0].id, "call-0");
    assert_eq!(sent[2].text(), "echo: hi");
    // The tool's start and end are journalled in order; live events show only
    // the requests, because the loop leaves a model call's tool events to the journal.
    let events: Vec<_> = f
        .host
        .journal
        .events()
        .into_iter()
        .map(|event| match event {
            JournalEvent::ToolStarted { call } => format!("started {}", call.id),
            JournalEvent::ToolFinished { call_id, .. } => format!("finished {call_id}"),
        })
        .collect();
    assert_eq!(events, ["started call-0", "finished call-0"]);
    let live = f.host.events.take();
    let requests_shown = live
        .iter()
        .filter(|event| matches!(event, LiveEvent::ModelRequestStarted))
        .count();
    assert_eq!(requests_shown, 2);
}

#[tokio::test]
async fn a_denied_call_does_not_run_and_the_model_reads_the_denial() {
    let f = fixture(echo_calls(1), None);
    let host = f.host.with_gate(Arc::new(DenyAll));
    let token = CancellationToken::new();
    let outcome = host.run("go", &LoopOptions::default(), &token).await;
    assert_eq!(outcome.status, "completed");
    assert!(f.tool.inputs.lock().unwrap().is_empty());
    let result = &host.journal.tool_results()[0];
    assert!(result.is_error);
    assert_eq!(result.text(), "denied by test gate");
    let requests = f.provider.requests.lock().unwrap();
    assert_eq!(requests[1].messages[2].text(), "denied by test gate");
}

#[tokio::test]
async fn the_round_limit_stops_a_model_that_keeps_calling_tools() {
    let f = fixture(echo_calls(10), None);
    let options = LoopOptions {
        max_rounds: Some(2),
        ..Default::default()
    };
    let outcome = f.host.run("go", &options, &CancellationToken::new()).await;
    assert_eq!(outcome.status, "failed");
    let error = outcome.error.unwrap();
    assert!(
        error.contains("Maximum model/tool rounds reached"),
        "{error}"
    );
    assert_eq!(f.tool.inputs.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn cancellation_ends_the_run_without_invoking_the_tool() {
    // Before the loop starts, and while the model call is in flight.
    for in_call in [false, true] {
        let token = CancellationToken::new();
        let f = fixture(echo_calls(1), in_call.then(|| token.clone()));
        if !in_call {
            token.cancel();
        }
        let outcome = f.host.run("go", &LoopOptions::default(), &token).await;
        assert_eq!(outcome.status, "cancelled", "in_call = {in_call}");
        assert!(
            f.tool.inputs.lock().unwrap().is_empty(),
            "in_call = {in_call}"
        );
        assert!(f.host.journal.is_closed(), "in_call = {in_call}");
        let sent = f.provider.requests.lock().unwrap().len();
        assert_eq!(sent, usize::from(in_call), "in_call = {in_call}");
    }
}
