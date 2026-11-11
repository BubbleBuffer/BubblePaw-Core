//! Tools-off reminder through each real adapter. A budgeted run's exhausted round
//! keeps the declarations and the wire tool choice when the model complies. If the
//! model still calls a tool, the next round is the enforcement round: the same
//! declarations with the provider's tool choice set off. Loopback HTTP, synthetic data only.
use async_trait::async_trait;
use axum::{
    Json, Router,
    extract::State,
    http::header,
    response::{IntoResponse, Response},
    routing::post,
};
use bubblepaw_core::{
    Harness, HarnessConfig, Provider, RunOptions, Store, Tool, ToolContext, ToolDefinition,
    ToolRegistry, ToolResult,
};
use bubblepaw_runner::provider::{ChatCompletionsProvider, MessagesProvider, ResponsesProvider};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

#[derive(Clone, Copy)]
enum Wire {
    Chat,
    Responses,
    Messages,
}

impl Wire {
    fn model(self) -> &'static str {
        match self {
            Self::Chat => "synthetic-reasoner",
            Self::Responses => "gpt-6-luna",
            Self::Messages => "claude-haiku-5-5",
        }
    }

    fn provider(self, base: String) -> Arc<dyn Provider> {
        match self {
            Self::Chat => {
                Arc::new(ChatCompletionsProvider::api_key("synthetic-key", base).unwrap())
            }
            Self::Responses => Arc::new(ResponsesProvider::api_key("synthetic-key", base).unwrap()),
            Self::Messages => Arc::new(MessagesProvider::api_key("synthetic-key", base).unwrap()),
        }
    }

    /// The wire tool choice of an enforced (tools-off) round.
    fn enforced_choice(self) -> Value {
        match self {
            Self::Chat | Self::Responses => json!("none"),
            Self::Messages => json!({"type":"none"}),
        }
    }

    /// A reply that calls `count`. Each call needs an id no earlier round used.
    fn call_reply(self, n: usize) -> String {
        match self {
            Self::Chat => format!(
                "{}data: [DONE]\n\n",
                sse(&[
                    json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":format!("call-{n}"),"type":"function","function":{"name":"count","arguments":"{}"}}]}}]}),
                    json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
                    json!({"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":5}}),
                ])
            ),
            Self::Responses => {
                let call = json!({"type":"function_call","call_id":format!("call-{n}"),"name":"count","arguments":"{}"});
                sse(&[
                    json!({"type":"response.output_item.done","output_index":0,"item":call}),
                    json!({"type":"response.completed","response":{"status":"completed","output":[call],"usage":{"input_tokens":6,"output_tokens":4}}}),
                ])
            }
            Self::Messages => sse(&[
                json!({"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":1}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":format!("toolu-{n}"),"name":"count","input":{}}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":5}}),
                json!({"type":"message_stop"}),
            ]),
        }
    }

    fn text_reply(self, text: &str) -> String {
        match self {
            Self::Chat => format!(
                "{}data: [DONE]\n\n",
                sse(&[
                    json!({"choices":[{"index":0,"delta":{"content":text}}]}),
                    json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
                    json!({"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":5}}),
                ])
            ),
            Self::Responses => sse(&[
                json!({"type":"response.output_text.delta","delta":text}),
                json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","id":"msg-1","role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]}],"usage":{"input_tokens":6,"output_tokens":4}}}),
            ]),
            Self::Messages => sse(&[
                json!({"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":1}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":5}}),
                json!({"type":"message_stop"}),
            ]),
        }
    }
}

fn sse(values: &[Value]) -> String {
    values
        .iter()
        .map(|value| format!("data: {value}\n\n"))
        .collect()
}

#[derive(Clone, Default)]
struct Mock {
    replies: Arc<Mutex<VecDeque<String>>>,
    bodies: Arc<Mutex<Vec<Value>>>,
}

async fn handle(State(mock): State<Mock>, Json(body): Json<Value>) -> Response {
    mock.bodies.lock().unwrap().push(body);
    let reply = mock.replies.lock().unwrap().pop_front().unwrap_or_default();
    ([(header::CONTENT_TYPE, "text/event-stream")], reply).into_response()
}

async fn start(replies: Vec<String>) -> (String, Mock) {
    let mock = Mock {
        replies: Arc::new(Mutex::new(replies.into())),
        bodies: Arc::default(),
    };
    let app = Router::new()
        .route("/messages", post(handle))
        .route("/chat/completions", post(handle))
        .route("/responses", post(handle))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, mock)
}

#[derive(Default)]
struct Count(AtomicUsize);

#[async_trait]
impl Tool for Count {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            approval: Default::default(),
            name: "count".into(),
            description: "count invocations".into(),
            read_only: true,
            input_schema: json!({"type":"object"}),
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> anyhow::Result<ToolResult> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::text("counted"))
    }
}

/// One budgeted run (one call allowed). Round 1 is the tools-off round: the model
/// answers in text (`ignore` false) or calls the tool again (`ignore` true), which
/// is refused and followed by the enforcement round. Returns the wire bodies.
async fn budget_run(wire: Wire, ignore: bool) -> Vec<Value> {
    let mut replies = vec![wire.call_reply(1)];
    replies.push(if ignore {
        wire.call_reply(2)
    } else {
        wire.text_reply("done")
    });
    if ignore {
        replies.push(wire.text_reply("done"));
    }
    let (base, mock) = start(replies).await;
    let count = Arc::new(Count::default());
    let mut registry = ToolRegistry::new();
    registry.register(count.clone()).unwrap();
    let store = Store::in_memory().unwrap();
    let session = store.create_session("tools off wire").unwrap();
    let harness = Harness::new(
        store.clone(),
        wire.provider(base),
        registry,
        HarnessConfig {
            system_prompt: "test".into(),
            ..Default::default()
        },
    );
    let run = harness
        .prompt(
            &session.id,
            "work",
            RunOptions {
                model: Some(wire.model().into()),
                max_tool_calls: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(
        count.0.load(Ordering::SeqCst),
        1,
        "only the budgeted call ran"
    );
    mock.bodies.lock().unwrap().clone()
}

fn assert_declared(body: &Value) {
    assert!(
        body["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty()),
        "round lost its declarations: {body}"
    );
}

fn assert_complied(bodies: &[Value]) {
    assert_eq!(bodies.len(), 2, "no enforcement round without a call");
    assert_declared(&bodies[0]);
    assert_eq!(bodies[1]["tools"], bodies[0]["tools"]);
    assert_eq!(bodies[1].get("tool_choice"), bodies[0].get("tool_choice"));
    assert!(
        bodies[1]
            .to_string()
            .contains("Tool calls are unavailable for the rest of this run"),
        "the tools-off reminder was not sent"
    );
}

fn assert_enforced(wire: Wire, bodies: &[Value]) {
    assert_eq!(
        bodies.len(),
        3,
        "one refused call, then one enforcement round"
    );
    assert_declared(&bodies[0]);
    assert_eq!(bodies[1]["tools"], bodies[0]["tools"]);
    assert_eq!(bodies[1].get("tool_choice"), bodies[0].get("tool_choice"));
    assert_eq!(bodies[2]["tools"], bodies[0]["tools"]);
    assert_eq!(bodies[2]["tool_choice"], wire.enforced_choice());
}

#[tokio::test]
async fn chat_tools_off_reminder_complies_without_a_tool_choice_change() {
    assert_complied(&budget_run(Wire::Chat, false).await);
}

#[tokio::test]
async fn chat_ignored_tools_off_reminder_is_enforced_with_tool_choice_none() {
    assert_enforced(Wire::Chat, &budget_run(Wire::Chat, true).await);
}

#[tokio::test]
async fn responses_tools_off_reminder_complies_without_a_tool_choice_change() {
    assert_complied(&budget_run(Wire::Responses, false).await);
}

#[tokio::test]
async fn responses_ignored_tools_off_reminder_is_enforced_with_tool_choice_none() {
    assert_enforced(Wire::Responses, &budget_run(Wire::Responses, true).await);
}

#[tokio::test]
async fn messages_tools_off_reminder_complies_without_a_tool_choice_change() {
    assert_complied(&budget_run(Wire::Messages, false).await);
}

#[tokio::test]
async fn messages_ignored_tools_off_reminder_is_enforced_with_tool_choice_none() {
    assert_enforced(Wire::Messages, &budget_run(Wire::Messages, true).await);
}
