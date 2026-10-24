#[path = "support/chat_messages.rs"]
mod chat_messages;
use axum::{Json, Router, extract::State, http::header, routing::post};
use bubblepaw_core::{
    CancellationToken, Message, MessageOrigin, ModelRequest, Provider, Role, ToolCall,
};
use bubblepaw_runner::provider::{ChatCompletionsProvider, MessagesProvider, ResponsesProvider};
use chat_messages::uuid_message;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Mock {
    requests: Arc<Mutex<Vec<Value>>>,
    reply: String,
}

async fn mock(reply: String) -> (String, Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
    let requests = Arc::new(Mutex::new(vec![]));
    let state = Mock {
        requests: requests.clone(),
        reply,
    };
    let router = Router::new()
        .route(
            "/{*path}",
            post(
                |State(state): State<Mock>, Json(body): Json<Value>| async move {
                    state.requests.lock().unwrap().push(body);
                    ([(header::CONTENT_TYPE, "text/event-stream")], state.reply)
                },
            ),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (base, requests, server)
}

fn events(values: &[Value]) -> String {
    values.iter().map(|v| format!("data: {v}\n\n")).collect()
}

fn notice() -> Message {
    let mut message = uuid_message(
        "session",
        Role::User,
        "[host notice]\nprocess:job exited exitCode=0 stdout=8 stderr=0",
    );
    message.origin = Some(MessageOrigin::Generated);
    message
}

fn history_with_tool_result() -> Vec<Message> {
    let mut assistant = uuid_message("session", Role::Assistant, "Starting.");
    assistant.tool_calls.push(ToolCall {
        id: "call".into(),
        name: "js".into(),
        arguments: json!({"code":"return 1"}),
    });
    let mut result = uuid_message("session", Role::Tool, "1");
    result.tool_call_id = Some("call".into());
    vec![
        notice(),
        uuid_message("session", Role::User, "Do the task"),
        assistant,
        result,
        notice(),
    ]
}

#[tokio::test]
async fn messages_merges_user_notices_preserving_tool_result_first() {
    let reply = events(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
        json!({"type":"message_stop"}),
    ]);
    let (base, captured, server) = mock(reply).await;
    let provider = MessagesProvider::api_key("synthetic", base).unwrap();
    let mut history = history_with_tool_result();
    let native_content = json!([
        {"type":"thinking","thinking":"Synthetic plan.","signature":"notice-sig+/="},
        {"type":"redacted_thinking","data":"notice-redacted+/="},
        {"type":"text","text":"Starting."},
        {"type":"tool_use","id":"call","name":"js","input":{"code":"return 1"}}
    ]);
    history[2].provider_state = Some(
        bubblepaw_core::with_provider_state_origin(
            json!({"adapter":"anthropic-messages","content":native_content}),
            "anthropic-messages",
            Some("mock"),
            "native",
        )
        .unwrap(),
    );
    provider
        .generate(
            ModelRequest {
                model: "mock".into(),
                messages: history,
                ..Default::default()
            },
            Arc::new(|_| {}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let captured = captured.lock().unwrap();
    let messages = captured[0]["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["content"][0]["text"], notice().content);
    assert_eq!(messages[0]["content"][1]["text"], "Do the task");
    assert_eq!(
        serde_json::to_vec(&messages[1]["content"]).unwrap(),
        serde_json::to_vec(&native_content).unwrap()
    );
    assert_eq!(messages[2]["content"][0]["type"], "tool_result");
    assert_eq!(messages[2]["content"][0]["tool_use_id"], "call");
    assert_eq!(messages[2]["content"][1]["text"], notice().content);
    server.abort();
}

#[tokio::test]
async fn responses_keeps_notice_after_function_output() {
    let (base, captured, server) = mock(events(&[json!({"type":"response.completed","response":{"status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":0}}})])).await;
    let provider = ResponsesProvider::api_key("synthetic", base).unwrap();
    provider
        .generate(
            ModelRequest {
                model: "mock".into(),
                messages: history_with_tool_result(),
                ..Default::default()
            },
            Arc::new(|_| {}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let captured = captured.lock().unwrap();
    let input = captured[0]["input"].as_array().unwrap();
    assert_eq!(input[0]["content"][0]["text"], notice().content);
    assert_eq!(input[1]["content"][0]["text"], "Do the task");
    assert_eq!(input[input.len() - 2]["type"], "function_call_output");
    assert_eq!(input[input.len() - 2]["call_id"], "call");
    assert_eq!(input.last().unwrap()["role"], "user");
    assert_eq!(
        input.last().unwrap()["content"][0]["text"],
        notice().content
    );
    server.abort();
}

#[tokio::test]
async fn chat_preserves_adjacent_notice_and_user_messages() {
    let (base, captured, server) = mock(format!(
        "{}data: [DONE]\n\n",
        events(&[
            json!({"choices":[{"index":0,"delta":{"content":"done"},"finish_reason":"stop"}]})
        ])
    ))
    .await;
    let provider = ChatCompletionsProvider::api_key("synthetic", base).unwrap();
    provider
        .generate(
            ModelRequest {
                model: "mock".into(),
                messages: vec![notice(), uuid_message("session", Role::User, "Do the task")],
                ..Default::default()
            },
            Arc::new(|_| {}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let captured = captured.lock().unwrap();
    assert_eq!(captured[0]["messages"][1]["content"], notice().content);
    assert_eq!(captured[0]["messages"][2]["content"], "Do the task");
    server.abort();
}
