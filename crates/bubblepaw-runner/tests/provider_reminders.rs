#[path = "support/chat_messages.rs"]
mod chat_messages;
use axum::{Router, body::Bytes, extract::State, http::header, routing::post};
use bubblepaw_core::{
    CancellationToken, Message, MessageOrigin, ModelRequest, Provider, Role, ToolCall,
};
use bubblepaw_runner::provider::{ChatCompletionsProvider, MessagesProvider};
use chat_messages::fixture_message;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

// A Generated user-role runtime reminder, as the host appends it between turns.
// P2 format: the wrapper alone, with no `[host notice]` prefix.
const REMINDER: &str = r#"<harness kind="runtime">live facts</harness>"#;

type Captured = Arc<Mutex<Vec<Vec<u8>>>>;

#[derive(Clone)]
struct Mock {
    captured: Captured,
    reply: String,
}

/// Captures the raw HTTP body bytes of every request and answers with `reply` as SSE.
async fn mock(reply: String) -> (String, Captured, tokio::task::JoinHandle<()>) {
    let captured: Captured = Arc::default();
    let state = Mock {
        captured: captured.clone(),
        reply,
    };
    let router = Router::new()
        .route(
            "/{*path}",
            post(|State(state): State<Mock>, bytes: Bytes| async move {
                state.captured.lock().unwrap().push(bytes.to_vec());
                (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    state.reply.clone(),
                )
            }),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (base, captured, server)
}

fn messages_reply() -> String {
    [
        json!({"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
        json!({"type":"message_stop"}),
    ]
    .iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect()
}

fn chat_reply() -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":{"content":"done"},"finish_reason":"stop"}]})
    )
}

fn reminder() -> Message {
    let mut message = fixture_message(Role::User, REMINDER);
    message.origin = Some(MessageOrigin::Generated);
    message
}

fn tool_result() -> Message {
    let mut result = fixture_message(Role::Tool, "1");
    result.tool_call_id = Some("call".into());
    result
}

/// Assistant turn that asked for one `js` call. `provider_state` is only
/// set for Messages, whose current tool loop must replay native blocks.
fn tool_call(provider_state: Option<Value>) -> Message {
    let mut assistant = fixture_message(Role::Assistant, "Starting.");
    assistant.tool_calls = vec![ToolCall {
        id: "call".into(),
        name: "js".into(),
        arguments: json!({"code":"return 1"}),
    }];
    assistant.provider_state = provider_state;
    assistant
}

fn native_tool_state() -> Value {
    let native = json!([
        {"type":"text","text":"Starting."},
        {"type":"tool_use","id":"call","name":"js","input":{"code":"return 1"}}
    ]);
    bubblepaw_core::with_provider_state_origin(
        json!({"adapter":"anthropic-messages","content":native}),
        "anthropic-messages",
        Some("mock"),
        "native",
    )
    .unwrap()
}

fn request(messages: Vec<Message>) -> ModelRequest {
    ModelRequest {
        model: "mock".into(),
        system_prompt: "System.".into(),
        messages,
        ..Default::default()
    }
}

fn body(captured: &Captured) -> Value {
    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 1);
    serde_json::from_slice(&requests[0]).unwrap()
}

/// JSON-escaped form of `text`, without the surrounding quotes, as it appears on the wire.
fn escaped(text: &str) -> String {
    let encoded = serde_json::to_string(text).unwrap();
    encoded[1..encoded.len() - 1].to_owned()
}

/// Byte offset of the first occurrence of `needle` in the raw request body.
fn at(raw: &[u8], needle: &str) -> usize {
    raw.windows(needle.len())
        .position(|window| window == needle.as_bytes())
        .unwrap_or_else(|| panic!("`{needle}` missing from wire body"))
}

fn wire(captured: &Captured) -> Vec<u8> {
    captured.lock().unwrap()[0].clone()
}

#[tokio::test]
async fn messages_reminder_after_plain_input_joins_the_user_turn() {
    let (base, captured, server) = mock(messages_reply()).await;
    let provider = MessagesProvider::api_key("synthetic", base).unwrap();
    provider
        .generate(
            request(vec![fixture_message(Role::User, "Do the task"), reminder()]),
            Arc::new(|_| {}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let sent = body(&captured);
    // The reminder never leaves the message list for the system array.
    let system = sent["system"].as_array().unwrap();
    assert_eq!(system.len(), 1);
    assert_eq!(system[0]["text"], "System.");
    // Adjacent user messages merge into one user turn, in order, with the wrapper intact.
    let messages = sent["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
    let turn = messages[0]["content"].as_array().unwrap();
    assert_eq!(turn.len(), 2);
    assert_eq!(turn[0]["type"], "text");
    assert_eq!(turn[0]["text"], "Do the task");
    assert_eq!(turn[1]["type"], "text");
    assert_eq!(turn[1]["text"], REMINDER);
    let raw = wire(&captured);
    assert!(
        !String::from_utf8_lossy(&raw).contains("[host notice]"),
        "reminder must carry no [host notice] prefix on the wire"
    );
    assert!(at(&raw, "Do the task") < at(&raw, &escaped(REMINDER)));
    assert_eq!(
        raw.windows(escaped(REMINDER).len())
            .filter(|window| *window == escaped(REMINDER).as_bytes())
            .count(),
        1
    );
    server.abort();
}

#[tokio::test]
async fn messages_reminder_after_tool_result_follows_it_in_the_same_turn() {
    let (base, captured, server) = mock(messages_reply()).await;
    let provider = MessagesProvider::api_key("synthetic", base).unwrap();
    let history = vec![
        fixture_message(Role::User, "Do the task"),
        tool_call(Some(native_tool_state())),
        tool_result(),
        reminder(),
    ];
    provider
        .generate(request(history), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    let sent = body(&captured);
    let system = sent["system"].as_array().unwrap();
    assert_eq!(system.len(), 1);
    assert_eq!(system[0]["text"], "System.");
    // user task, assistant native tool turn, then one user turn holding result + reminder.
    let messages = sent["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"][0]["text"], "Do the task");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(
        messages[1]["content"],
        json!([
            {"type":"text","text":"Starting."},
            {"type":"tool_use","id":"call","name":"js","input":{"code":"return 1"}}
        ])
    );
    assert_eq!(messages[2]["role"], "user");
    let turn = messages[2]["content"].as_array().unwrap();
    assert_eq!(turn.len(), 2);
    assert_eq!(
        turn[0],
        json!({"type":"tool_result","tool_use_id":"call","content":"1","is_error":false})
    );
    assert_eq!(turn[1]["type"], "text");
    assert_eq!(turn[1]["text"], REMINDER);
    // Byte order on the wire: the tool_result block precedes the reminder text.
    let raw = wire(&captured);
    assert!(
        !String::from_utf8_lossy(&raw).contains("[host notice]"),
        "reminder must carry no [host notice] prefix on the wire"
    );
    assert!(at(&raw, r#""tool_use_id":"call""#) < at(&raw, &escaped(REMINDER)));
    server.abort();
}

#[tokio::test]
async fn chat_reminder_after_plain_input_is_a_separate_user_message() {
    let (base, captured, server) = mock(chat_reply()).await;
    let provider = ChatCompletionsProvider::api_key("synthetic", base).unwrap();
    provider
        .generate(
            request(vec![fixture_message(Role::User, "Do the task"), reminder()]),
            Arc::new(|_| {}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let sent = body(&captured);
    assert!(sent.get("system").is_none());
    let messages = sent["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "System.");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "Do the task");
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"], REMINDER);
    assert_eq!(
        messages.iter().filter(|m| m["role"] == "system").count(),
        1,
        "reminder hoisted into a system message"
    );
    let raw = wire(&captured);
    assert!(
        !String::from_utf8_lossy(&raw).contains("[host notice]"),
        "reminder must carry no [host notice] prefix on the wire"
    );
    assert!(at(&raw, "Do the task") < at(&raw, &escaped(REMINDER)));
    server.abort();
}

#[tokio::test]
async fn chat_reminder_after_tool_result_keeps_tool_adjacency() {
    let (base, captured, server) = mock(chat_reply()).await;
    let provider = ChatCompletionsProvider::api_key("synthetic", base).unwrap();
    let history = vec![
        fixture_message(Role::User, "Do the task"),
        tool_call(None),
        tool_result(),
        reminder(),
    ];
    provider
        .generate(request(history), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    let sent = body(&captured);
    assert!(sent.get("system").is_none());
    let messages = sent["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 5);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "System.");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "Do the task");
    // The tool call and its result stay adjacent; the reminder follows the result.
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["tool_calls"][0]["id"], "call");
    assert_eq!(messages[2]["tool_calls"][0]["function"]["name"], "js");
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["tool_call_id"], "call");
    assert_eq!(messages[3]["content"], "1");
    assert_eq!(messages[4]["role"], "user");
    assert_eq!(messages[4]["content"], REMINDER);
    assert!(messages[4].get("tool_call_id").is_none());
    assert_eq!(
        messages.iter().filter(|m| m["role"] == "system").count(),
        1,
        "reminder hoisted into a system message"
    );
    let raw = wire(&captured);
    assert!(
        !String::from_utf8_lossy(&raw).contains("[host notice]"),
        "reminder must carry no [host notice] prefix on the wire"
    );
    assert!(at(&raw, r#""tool_call_id":"call""#) < at(&raw, &escaped(REMINDER)));
    server.abort();
}
