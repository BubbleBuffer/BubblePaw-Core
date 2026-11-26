//! Each HTTP adapter reports the serving model, the stop reason and the usage of
//! every physical request as typed fields of the reply. The mock serves scripted
//! server-sent event replies in order.
use axum::{
    Router,
    extract::State,
    http::header,
    response::{IntoResponse, Response},
    routing::post,
};
use bubblepaw_core::{
    CancellationToken, Message, ModelRequest, ModelResponse, Provider, Role, StopReason,
    StreamCallback, StreamEvent, ToolDefinition,
};
use bubblepaw_runner::provider::{ChatCompletionsProvider, MessagesProvider, ResponsesProvider};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

type Replies = Arc<Mutex<std::collections::VecDeque<String>>>;

async fn handle(State(replies): State<Replies>) -> Response {
    let reply = replies.lock().unwrap().pop_front().unwrap_or_default();
    ([(header::CONTENT_TYPE, "text/event-stream")], reply).into_response()
}

fn sse(values: &[Value], done: bool) -> String {
    let events: String = values.iter().map(|v| format!("data: {v}\n\n")).collect();
    if done {
        format!("{events}data: [DONE]\n\n")
    } else {
        events
    }
}

fn messages(model: Option<&str>, stop: &str, output: u64) -> String {
    sse(
        &[
            json!({"type":"message_start","message":{"model":model,"usage":{"input_tokens":10,"output_tokens":1}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":stop},"usage":{"output_tokens":output}}),
            json!({"type":"message_stop"}),
        ],
        false,
    )
}

fn messages_tool() -> String {
    sse(
        &[
            json!({"type":"message_start","message":{"model":"claude-x","usage":{"input_tokens":10,"output_tokens":1}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"workspace_read","input":{}}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}),
            json!({"type":"message_stop"}),
        ],
        false,
    )
}

fn chat(model: Option<&str>, finish: &str, output: u64) -> String {
    sse(
        &[
            json!({"model":model,"choices":[{"index":0,"delta":{"content":"hi"}}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]}),
            json!({"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":output}}),
        ],
        true,
    )
}

fn chat_tool() -> String {
    sse(
        &[
            json!({"model":"gpt-x","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"workspace_read","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":3}}),
        ],
        true,
    )
}

fn responses(model: Option<&str>, status: &str, reason: Option<&str>, output: Value) -> String {
    let details = reason.map(|reason| json!({ "reason": reason }));
    let event = if status == "completed" {
        "response.completed"
    } else {
        "response.incomplete"
    };
    sse(
        &[
            json!({"type":"response.output_text.delta","delta":"hi"}),
            json!({"type":event,"response":{"model":model,"status":status,"incomplete_details":details,"output":output,"usage":{"input_tokens":6,"output_tokens":4}}}),
        ],
        false,
    )
}

fn tool_call() -> Value {
    json!([{"type":"function_call","call_id":"c1","name":"workspace_read","arguments":"{}"}])
}

async fn run(kind: &str, replies: Vec<String>) -> ModelResponse {
    let replies: Replies = Arc::new(Mutex::new(replies.into()));
    let app = Router::new()
        .route("/messages", post(handle))
        .route("/chat/completions", post(handle))
        .route("/responses", post(handle))
        .with_state(replies);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let provider: Box<dyn Provider> = match kind {
        "messages" => Box::new(MessagesProvider::api_key("synthetic-key", base).unwrap()),
        "chat" => Box::new(ChatCompletionsProvider::api_key("synthetic-key", base).unwrap()),
        _ => Box::new(ResponsesProvider::api_key("synthetic-key", base).unwrap()),
    };
    let request = ModelRequest {
        model: "configured".into(),
        system_prompt: "test".into(),
        messages: vec![Message {
            role: Role::User,
            content: bubblepaw_core::text_blocks("go"),
            ..Default::default()
        }],
        tools: vec![ToolDefinition {
            approval: Default::default(),
            name: "workspace_read".into(),
            description: "read".into(),
            read_only: true,
            input_schema: json!({"type":"object"}),
        }],
        ..Default::default()
    };
    let noop: StreamCallback = Arc::new(|_: StreamEvent| {});
    provider
        .generate(request, noop, CancellationToken::new())
        .await
        .unwrap()
}

/// What a reply reports: serving model, stop reason, and (input, output) tokens of each request.
type Facts = (Option<String>, Option<StopReason>, Vec<(u64, u64)>);

fn facts(response: &ModelResponse) -> Facts {
    let usages = response.request_usages.iter();
    (
        response.model.clone(),
        response.stop_reason.clone(),
        usages.map(|u| (u.input_tokens, u.output_tokens)).collect(),
    )
}

#[tokio::test]
async fn adapters_report_model_stop_reason_and_usage_per_request() {
    let some = |model: &str| Some(model.to_owned());
    let other = |raw: &str| Some(StopReason::Other(raw.into()));
    let end = Some(StopReason::EndTurn);
    #[rustfmt::skip]
    let cases: Vec<(&str, Vec<String>, Facts)> = vec![
        ("messages", vec![messages(Some("claude-x"), "end_turn", 7)], (some("claude-x"), end.clone(), vec![(10, 7)])),
        ("messages", vec![messages_tool()], (some("claude-x"), Some(StopReason::ToolUse), vec![(10, 5)])),
        ("messages", vec![messages(None, "stop_sequence", 7)], (None, other("stop_sequence"), vec![(10, 7)])),
        ("messages", vec![messages(Some("claude-x"), "max_tokens", 50), messages(Some("claude-x"), "end_turn", 7)], (some("claude-x"), end.clone(), vec![(10, 50), (10, 7)])),
        ("chat", vec![chat(Some("gpt-x"), "stop", 5)], (some("gpt-x"), end.clone(), vec![(8, 5)])),
        ("chat", vec![chat_tool()], (some("gpt-x"), Some(StopReason::ToolUse), vec![(8, 3)])),
        ("chat", vec![chat(None, "content_filter", 5)], (None, Some(StopReason::ContentFilter), vec![(8, 5)])),
        ("chat", vec![chat(Some("gpt-x"), "length", 40), chat(Some("gpt-x"), "stop", 5)], (some("gpt-x"), end.clone(), vec![(8, 40), (8, 5)])),
        ("responses", vec![responses(None, "completed", None, json!([]))], (None, end.clone(), vec![(6, 4)])),
        ("responses", vec![responses(Some("resp-x"), "completed", None, json!([]))], (some("resp-x"), end.clone(), vec![(6, 4)])),
        ("responses", vec![responses(Some("resp-x"), "completed", None, tool_call())], (some("resp-x"), Some(StopReason::ToolUse), vec![(6, 4)])),
        ("responses", vec![responses(Some("resp-x"), "incomplete", Some("max_output_tokens"), json!([])), responses(Some("resp-x"), "completed", None, json!([]))], (some("resp-x"), end, vec![(6, 4), (6, 4)])),
    ];
    for (index, (kind, replies, expected)) in cases.into_iter().enumerate() {
        let response = run(kind, replies).await;
        assert_eq!(facts(&response), expected, "case {index} ({kind})");
    }
}
