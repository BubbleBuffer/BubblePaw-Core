//! Chat Completions and Responses have no error field on tool results, so an
//! error result is sent as text starting with `Error: ` (once). The stored
//! message keeps its content.
use axum::{Json, Router, extract::State, routing::post};
use bubblepaw_core::{CancellationToken, Message, ModelRequest, Provider, Role, ToolCall};
use bubblepaw_runner::provider::{ChatCompletionsProvider, ResponsesProvider};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

type Seen = Arc<Mutex<Vec<Value>>>;

fn message(role: Role, content: &str) -> Message {
    Message {
        id: format!("m-{content}"),
        session_id: "session".into(),
        role,
        content: content.into(),
        ..Default::default()
    }
}

fn request(result: &str, is_error: bool) -> ModelRequest {
    let mut assistant = message(Role::Assistant, "calling");
    assistant.tool_calls = vec![ToolCall {
        id: "call_a".into(),
        name: "read".into(),
        arguments: json!({}),
    }];
    let mut tool = message(Role::Tool, result);
    tool.tool_call_id = Some("call_a".into());
    tool.is_error = is_error;
    ModelRequest {
        model: "model".into(),
        system_prompt: "System.".into(),
        messages: vec![message(Role::User, "Hello."), assistant, tool],
        ..Default::default()
    }
}

async fn server() -> (String, Seen) {
    let seen = Seen::default();
    let app = Router::new()
        .route(
            "/chat/completions",
            post(|State(seen): State<Seen>, Json(body): Json<Value>| async move {
                seen.lock().unwrap().push(body);
                Json(json!({"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}))
            }),
        )
        .route(
            "/responses",
            post(|State(seen): State<Seen>, Json(body): Json<Value>| async move {
                seen.lock().unwrap().push(body);
                let response = json!({"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}]});
                ([("content-type", "text/event-stream")], format!("data: {}\n\n", json!({"type":"response.completed","response":response})))
            }),
        )
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, seen)
}

#[tokio::test]
async fn error_results_start_with_error_on_adapters_without_an_error_field() {
    let cases = [
        ("file missing", true, "Error: file missing"),
        ("error: file missing", true, "error: file missing"),
        ("ERROR file missing", true, "ERROR file missing"),
        ("file missing", false, "file missing"),
    ];
    let (base, seen) = server().await;
    let chat = ChatCompletionsProvider::api_key("synthetic-key", &base).unwrap();
    let responses = ResponsesProvider::api_key("synthetic-key", base).unwrap();
    for (content, is_error, wire) in cases {
        let sent = request(content, is_error);
        for provider in [&chat as &dyn Provider, &responses] {
            provider
                .generate(sent.clone(), Arc::new(|_| {}), CancellationToken::new())
                .await
                .unwrap();
        }
        let seen = seen.lock().unwrap();
        let (chat_body, responses_body) = (&seen[seen.len() - 2], &seen[seen.len() - 1]);
        assert_eq!(chat_body["messages"][3]["content"], wire, "{content}");
        let output = responses_body["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call_output")
            .unwrap();
        assert_eq!(output["output"], wire, "{content}");
        assert_eq!(sent.messages[2].content, content);
    }
}
