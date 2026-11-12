//! Real Harness tool rounds through loopback HTTP, reloading SQLite on every step.
//! Synthetic opaque data only; no model service or credentials are used.
use anyhow::{Result, ensure};
use async_trait::async_trait;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use bubblepaw_core::{
    CancellationToken, Capabilities, ContextPreparation, Harness, HarnessConfig, Message,
    ModelRequest, ModelResponse, Provider, ProviderError, ProviderErrorKind, Role, RunOptions,
    Store, TextCallback, Tool, ToolContext, ToolDefinition, ToolRegistry, ToolResult,
};
use bubblepaw_runner::provider::{ChatCompletionsProvider, MessagesProvider, ResponsesProvider};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

const TOOL_ROUNDS: usize = 3;

#[derive(Clone, Copy, Debug)]
enum Kind {
    Messages,
    Responses,
    Chat,
}

impl Kind {
    fn model(self) -> &'static str {
        match self {
            Self::Messages => "claude-haiku-5-5",
            Self::Responses => "gpt-6-luna",
            Self::Chat => "synthetic-chat-reasoner",
        }
    }

    fn field(self) -> &'static str {
        match self {
            Self::Messages => "content",
            Self::Responses => "items",
            Self::Chat => "message_fields",
        }
    }

    fn opaque(self, round: usize) -> Value {
        // Each turn exceeds the provider-state blob threshold. Include escaping,
        // Unicode and unknown fields so text reconstruction is not sufficient.
        let signed = format!("round-{round} +/== 猫🐾\n\t\\\" ").repeat(320);
        match self {
            Self::Messages => json!([
                {"type":"thinking","thinking":format!("Plan {round}: π\n雪"),
                 "signature":signed,"future":{"keep":true,"signed_zero":-0.0}},
                {"type":"redacted_thinking","data":format!("redacted-{signed}"),
                 "future":[null,true,{"猫":"🐾"}]}
            ]),
            Self::Responses => json!([
                {"type":"reasoning","id":format!("rs-{round}"),"encrypted_content":signed,
                 "summary":[],"future":{"keep":true,"signed_zero":-0.0}}
            ]),
            Self::Chat => json!({"reasoning_content":signed}),
        }
    }

    fn blocks(self, round: usize) -> Vec<Value> {
        let mut blocks = self.opaque(round).as_array().unwrap().clone();
        let text = visible(round);
        match self {
            Self::Messages => {
                blocks.push(json!({"type":"text","text":text}));
                if round < TOOL_ROUNDS {
                    blocks.push(json!({"type":"tool_use","id":call_id(round),"name":"echo",
                        "input":{"round":round}}));
                }
            }
            Self::Responses => {
                blocks.push(json!({"type":"message","id":format!("msg-{round}"),
                    "role":"assistant","status":"completed",
                    "content":[{"type":"output_text","text":text,"annotations":[]}]}));
                if round < TOOL_ROUNDS {
                    blocks.push(json!({"type":"function_call","id":format!("fc-{round}"),
                        "call_id":call_id(round),"name":"echo",
                        "arguments":format!("{{ \"round\" : {round} }}")}));
                }
            }
            Self::Chat => unreachable!(),
        }
        blocks
    }

    fn reply(self, round: usize) -> String {
        let events = match self {
            Self::Messages => {
                let mut events = vec![json!({"type":"message_start","message":{
                    "usage":{"input_tokens":8,"output_tokens":1}}})];
                for (index, block) in self.blocks(round).into_iter().enumerate() {
                    let mut start = block.clone();
                    let fields: &[(&str, &str)] = match block["type"].as_str().unwrap() {
                        "thinking" => &[
                            ("thinking", "thinking_delta"),
                            ("signature", "signature_delta"),
                        ],
                        "text" => &[("text", "text_delta")],
                        "tool_use" => &[("input", "input_json_delta")],
                        _ => &[],
                    };
                    for (field, _) in fields {
                        start[*field] = if *field == "input" {
                            json!({})
                        } else {
                            json!("")
                        };
                    }
                    events.push(
                        json!({"type":"content_block_start","index":index,"content_block":start}),
                    );
                    for (field, kind) in fields {
                        let raw = if *field == "input" {
                            block[*field].to_string()
                        } else {
                            block[*field].as_str().unwrap().to_owned()
                        };
                        // Force opaque signatures through multiple stream fragments.
                        let split = raw
                            .char_indices()
                            .nth(raw.chars().count() / 2)
                            .map(|(offset, _)| offset)
                            .unwrap_or(raw.len());
                        for fragment in [&raw[..split], &raw[split..]] {
                            let mut delta = json!({"type":kind});
                            delta[if *field == "input" {
                                "partial_json"
                            } else {
                                field
                            }] = json!(fragment);
                            events.push(
                                json!({"type":"content_block_delta","index":index,"delta":delta}),
                            );
                        }
                    }
                    events.push(json!({"type":"content_block_stop","index":index}));
                }
                events.push(json!({"type":"message_delta","delta":{"stop_reason":
                    if round < TOOL_ROUNDS {"tool_use"} else {"end_turn"}},"usage":{"output_tokens":5}}));
                events.push(json!({"type":"message_stop"}));
                events
            }
            Self::Responses => vec![json!({"type":"response.completed","response":{
                "status":"completed","output":self.blocks(round),
                "usage":{"input_tokens":8,"output_tokens":5}}})],
            Self::Chat => {
                let opaque = self.opaque(round);
                let raw = opaque["reasoning_content"].as_str().unwrap();
                let split = raw.char_indices().nth(raw.chars().count() / 2).unwrap().0;
                let mut events = vec![
                    json!({"choices":[{"index":0,"delta":{"reasoning_content":&raw[..split]}}]}),
                    json!({"choices":[{"index":0,"delta":{"reasoning_content":&raw[split..],"content":visible(round)}}]}),
                ];
                if round < TOOL_ROUNDS {
                    events.push(json!({"choices":[{"index":0,"delta":{"tool_calls":[{
                        "index":0,"id":call_id(round),"type":"function",
                        "function":{"name":"echo","arguments":format!("{{\"round\":{round}}}")}}]}}]}));
                }
                events.push(json!({"choices":[{"index":0,"delta":{},"finish_reason":
                    if round < TOOL_ROUNDS {"tool_calls"} else {"stop"}}]}));
                events
                    .push(json!({"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":5}}));
                events
            }
        };
        let mut stream: String = events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect();
        if matches!(self, Self::Chat) {
            stream.push_str("data: [DONE]\n\n");
        }
        stream
    }
}

fn call_id(round: usize) -> String {
    format!("call-{round}")
}
fn visible(round: usize) -> String {
    if round == TOOL_ROUNDS {
        "done".into()
    } else {
        format!("Calling round {round}: 猫\n")
    }
}
fn tool_output(round: usize) -> String {
    format!("echo result {round}: π🐾")
}

fn byte_equal(actual: &Value, expected: &Value, boundary: &str) -> Result<()> {
    ensure!(
        serde_json::to_vec(actual)? == serde_json::to_vec(expected)?,
        "opaque reasoning changed at {boundary}"
    );
    Ok(())
}

fn check_stored(kind: Kind, message: &Message, round: usize) -> Result<()> {
    ensure!(
        message.role == Role::Assistant && message.content == visible(round),
        "assistant changed"
    );
    let state = message
        .provider_state
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing durable state"))?;
    ensure!(
        state["_bubblepaw_origin"]["model"] == kind.model(),
        "model origin changed"
    );
    let opaque = match kind {
        Kind::Chat => state[kind.field()].clone(),
        _ => json!(
            state[kind.field()]
                .as_array()
                .unwrap()
                .iter()
                .filter(|block| matches!(
                    block["type"].as_str(),
                    Some("thinking" | "redacted_thinking" | "reasoning")
                ))
                .cloned()
                .collect::<Vec<_>>()
        ),
    };
    byte_equal(&opaque, &kind.opaque(round), "SQLite read")?;
    if round < TOOL_ROUNDS {
        ensure!(
            message.tool_calls.len() == 1
                && message.tool_calls[0].id == call_id(round)
                && message.tool_calls[0].arguments == json!({"round":round}),
            "durable call changed"
        );
    } else {
        ensure!(message.tool_calls.is_empty(), "final turn has calls");
    }
    Ok(())
}

struct ReloadContext {
    store: Store,
    session: String,
    reads: AtomicUsize,
    erase_state: bool,
}
#[async_trait]
impl ContextPreparation for ReloadContext {
    fn project_history(&self, incoming: &[Message]) -> Result<Vec<Message>> {
        // This is Store::messages' SELECT + blob hydration, not ParsedHistory.
        // Return the reread rows so every request actually uses SQLite state.
        let mut persisted = self.store.messages(&self.session)?;
        ensure!(
            serde_json::to_vec(&persisted)? == serde_json::to_vec(incoming)?,
            "runtime cached history differs from SQLite"
        );
        self.reads.fetch_add(1, Ordering::SeqCst);
        if self.erase_state {
            for message in &mut persisted {
                if message.role == Role::Assistant && !message.tool_calls.is_empty() {
                    message.provider_state = None;
                }
            }
        }
        Ok(persisted)
    }
}

#[derive(Clone)]
struct Fake {
    kind: Kind,
    store: Store,
    session: String,
    requests: Arc<Mutex<Vec<Value>>>,
    failures: Arc<Mutex<Vec<String>>>,
}
impl Fake {
    fn verify(&self, body: &Value, round: usize) -> Result<()> {
        ensure!(round <= TOOL_ROUNDS, "unexpected additional model call");
        let (field, expected_length) = match self.kind {
            Kind::Messages => ("messages", 1 + round * 2),
            Kind::Responses => ("input", 1 + round * 4),
            Kind::Chat => ("messages", 2 + round * 2),
        };
        ensure!(
            body[field]
                .as_array()
                .is_some_and(|items| items.len() == expected_length),
            "native wire turns were omitted or duplicated"
        );
        let stored = self.store.messages(&self.session)?;
        ensure!(
            stored.len() == 1 + round * 2 && stored[0].role == Role::User,
            "followup arrived before its messages committed"
        );
        for previous in 0..round {
            check_stored(self.kind, &stored[1 + previous * 2], previous)?;
            let result = &stored[2 + previous * 2];
            ensure!(
                result.role == Role::Tool
                    && result.tool_call_id.as_deref() == Some(&call_id(previous))
                    && result.content == tool_output(previous),
                "tool result order changed"
            );
            match self.kind {
                Kind::Messages => {
                    let messages = body["messages"].as_array().unwrap();
                    byte_equal(
                        &messages[1 + previous * 2]["content"],
                        &json!(self.kind.blocks(previous)),
                        "Messages followup wire",
                    )?;
                    ensure!(
                        messages[2 + previous * 2]["content"][0]["type"] == "tool_result"
                            && messages[2 + previous * 2]["content"][0]["tool_use_id"]
                                == call_id(previous)
                            && messages[2 + previous * 2]["content"][0]["content"]
                                == tool_output(previous),
                        "Messages result follows wrong call"
                    );
                }
                Kind::Responses => {
                    let input = body["input"].as_array().unwrap();
                    let start = 1 + previous * 4;
                    byte_equal(
                        &json!(&input[start..start + 3]),
                        &json!(self.kind.blocks(previous)),
                        "Responses followup wire",
                    )?;
                    ensure!(
                        input[start + 3]["type"] == "function_call_output"
                            && input[start + 3]["call_id"] == call_id(previous)
                            && input[start + 3]["output"] == tool_output(previous),
                        "Responses result follows wrong call"
                    );
                }
                Kind::Chat => {
                    let messages = body["messages"].as_array().unwrap();
                    let assistant = &messages[2 + previous * 2]; // Includes the system prompt.
                    byte_equal(
                        &assistant["reasoning_content"],
                        &self.kind.opaque(previous)["reasoning_content"],
                        "chat followup wire",
                    )?;
                    ensure!(
                        assistant["tool_calls"][0]["id"] == call_id(previous)
                            && messages[3 + previous * 2]["tool_call_id"] == call_id(previous)
                            && messages[3 + previous * 2]["content"] == tool_output(previous),
                        "chat call/result order changed"
                    );
                }
            }
        }
        Ok(())
    }
}

async fn handle(State(fake): State<Fake>, Json(body): Json<Value>) -> Response {
    let round = {
        let mut requests = fake.requests.lock().unwrap();
        let round = requests.len();
        requests.push(body.clone());
        round
    };
    if let Err(error) = fake.verify(&body, round) {
        fake.failures.lock().unwrap().push(error.to_string());
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"synthetic replay assertion failed"})),
        )
            .into_response();
    }
    (
        [("content-type", "text/event-stream")],
        fake.kind.reply(round),
    )
        .into_response()
}

struct Echo {
    kind: Kind,
    store: Store,
    calls: AtomicUsize,
}

struct AuditedProvider {
    inner: Arc<dyn Provider>,
    errors: Arc<Mutex<Vec<ProviderError>>>,
}
#[async_trait]
impl Provider for AuditedProvider {
    fn capabilities(&self, model: &str) -> Capabilities {
        self.inner.capabilities(model)
    }
    async fn generate(
        &self,
        request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let result = self.inner.generate(request, on_text, cancellation).await;
        if let Err(error) = &result
            && let Some(classified) = ProviderError::of(error)
        {
            self.errors.lock().unwrap().push(classified.clone());
        }
        result
    }
}
#[async_trait]
impl Tool for Echo {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "echo".into(),
            description: "Synthetic read-only echo".into(),
            read_only: true,
            approval: Default::default(),
            input_schema: json!({"type":"object","properties":{"round":{"type":"integer"}},"required":["round"],"additionalProperties":false}),
        }
    }
    async fn invoke(&self, context: ToolContext, input: Value) -> Result<ToolResult> {
        let round = self.calls.fetch_add(1, Ordering::SeqCst);
        ensure!(
            input == json!({"round":round}),
            "tool executed out of order"
        );
        let stored = self.store.messages(&context.session_id)?;
        ensure!(
            stored.len() == 2 + round * 2,
            "tool started before assistant commit"
        );
        check_stored(self.kind, stored.last().unwrap(), round)?;
        Ok(ToolResult::text(tool_output(round)))
    }
}

async fn run_fixture(kind: Kind, erase_state: bool) -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = Store::open(root.path())?;
    let session = store.create_session("synthetic reasoning loop")?.id;
    let fake = Fake {
        kind,
        store: store.clone(),
        session: session.clone(),
        requests: Arc::default(),
        failures: Arc::default(),
    };
    let app = Router::new()
        .route("/messages", post(handle))
        .route("/responses", post(handle))
        .route("/chat/completions", post(handle))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let provider: Arc<dyn Provider> = match kind {
        Kind::Messages => Arc::new(MessagesProvider::api_key("synthetic-key", base)?),
        Kind::Responses => Arc::new(ResponsesProvider::api_key("synthetic-key", base)?),
        // Exercise the default current-loop replay policy, without an override.
        Kind::Chat => Arc::new(ChatCompletionsProvider::api_key("synthetic-key", base)?),
    };
    let provider_errors = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(AuditedProvider {
        inner: provider,
        errors: provider_errors.clone(),
    });
    let context = Arc::new(ReloadContext {
        store: store.clone(),
        session: session.clone(),
        reads: AtomicUsize::new(0),
        erase_state,
    });
    let echo = Arc::new(Echo {
        kind,
        store: store.clone(),
        calls: AtomicUsize::new(0),
    });
    let mut tools = ToolRegistry::new();
    tools.register(echo.clone())?;
    let harness = Harness::new(
        store.clone(),
        provider,
        tools,
        HarnessConfig {
            model: kind.model().into(),
            max_rounds: Some(8),
            context: context.clone(),
            ..Default::default()
        },
    );
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        harness.prompt(
            &session,
            "Run three echo rounds, then answer.",
            RunOptions::default(),
        ),
    )
    .await;
    server.abort();
    let run = result??;
    let failures = fake.failures.lock().unwrap().clone();
    ensure!(
        failures.is_empty(),
        "fake transport assertions: {failures:?}"
    );
    if erase_state {
        let errors = provider_errors.lock().unwrap();
        ensure!(
            errors.len() == 1
                && errors[0].kind == ProviderErrorKind::Fatal
                && !errors[0].retryable(),
            "missing state was not a single local Fatal error"
        );
        ensure!(
            run.status == "failed",
            "missing-state fixture unexpectedly completed: {run:?}"
        );
        ensure!(
            run.error
                .as_deref()
                .unwrap_or("")
                .contains("current thinking tool loop")
                && run
                    .error
                    .as_deref()
                    .unwrap_or("")
                    .contains("missing provider_state"),
            "unclear local guard error: {run:?}"
        );
        ensure!(
            fake.requests.lock().unwrap().len() == 1,
            "missing state reached followup HTTP"
        );
        ensure!(
            echo.calls.load(Ordering::SeqCst) == 1 && context.reads.load(Ordering::SeqCst) == 2,
            "fixture did not reach the intended tool boundary"
        );
        check_stored(kind, &store.messages(&session)?[1], 0)?;
    } else {
        ensure!(
            run.status == "completed" && run.text.as_deref() == Some("done"),
            "runtime failed: {run:?}"
        );
        ensure!(
            fake.requests.lock().unwrap().len() == TOOL_ROUNDS + 1,
            "wrong model round count"
        );
        ensure!(
            echo.calls.load(Ordering::SeqCst) == TOOL_ROUNDS
                && context.reads.load(Ordering::SeqCst) == TOOL_ROUNDS + 1,
            "wrong SQLite reload/tool counts"
        );
        let stored = store.messages(&session)?;
        ensure!(
            stored.len() == 2 + TOOL_ROUNDS * 2,
            "final response not durable"
        );
        for round in 0..=TOOL_ROUNDS {
            check_stored(kind, &stored[1 + round * 2], round)?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn messages_thinking_and_redacted_thinking_survive_three_durable_tool_rounds() -> Result<()> {
    run_fixture(Kind::Messages, false).await
}
#[tokio::test]
async fn responses_encrypted_content_survives_three_durable_tool_rounds() -> Result<()> {
    run_fixture(Kind::Responses, false).await
}
#[tokio::test]
async fn chat_default_replays_reasoning_content_after_three_durable_tool_rounds() -> Result<()> {
    run_fixture(Kind::Chat, false).await
}
#[tokio::test]
async fn missing_current_messages_state_fails_locally_before_followup_http() -> Result<()> {
    run_fixture(Kind::Messages, true).await
}
