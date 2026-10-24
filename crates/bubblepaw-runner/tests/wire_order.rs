//! P5: cache tiers steer only the Messages wire. Chat and Responses keep every
//! system message in position and carry no cache markers.
#[path = "support/chat_messages.rs"]
mod chat_messages;
use axum::{Json, Router, http::StatusCode, routing::post};
use bubblepaw_core::{CacheTiers, CancellationToken, ModelRequest, Provider, Role};
use bubblepaw_runner::provider::{ChatCompletionsProvider, ResponsesProvider};
use chat_messages::fixture_message;
use serde_json::Value;
use std::sync::{Arc, Mutex};

fn request() -> ModelRequest {
    ModelRequest {
        model: "mock-model".into(),
        system_prompt: "Static prompt.".into(),
        messages: vec![
            fixture_message(Role::System, "View block."),
            fixture_message(Role::User, "First question."),
            fixture_message(Role::Assistant, "First answer."),
            fixture_message(Role::System, "Cold note."),
            fixture_message(Role::User, "Second question."),
        ],
        cache_tiers: Some(CacheTiers {
            cold_end: Some(1),
            previous_len: Some(3),
        }),
        ..Default::default()
    }
}

/// (role, text) of each wire item, in order. Content is a string (Chat) or
/// a list of text parts (Responses).
fn order(items: &Value) -> Vec<(String, String)> {
    items
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            let content = &item["content"];
            let text = content.as_str().or_else(|| content[0]["text"].as_str());
            (item["role"].to_string(), text.unwrap().to_owned())
        })
        .collect()
}

fn expected(with_prompt: bool) -> Vec<(String, String)> {
    let mut items = vec![
        ("system", "View block."),
        ("user", "First question."),
        ("assistant", "First answer."),
        ("system", "Cold note."),
        ("user", "Second question."),
    ];
    if with_prompt {
        items.insert(0, ("system", "Static prompt."));
    }
    items
        .into_iter()
        .map(|(role, text)| (format!("{role:?}"), text.to_owned()))
        .collect()
}

#[test]
fn chat_keeps_system_messages_in_position_without_cache_markers() {
    let provider = ChatCompletionsProvider::api_key("synthetic-key", "http://127.0.0.1:1").unwrap();
    let wire = Mutex::new(Value::Null);
    provider
        .estimate_input_tokens(&request(), &|input| {
            *wire.lock().unwrap() = input.clone();
            0
        })
        .unwrap();
    let wire = wire.into_inner().unwrap();
    assert_eq!(order(&wire["messages"]), expected(true));
    assert!(!wire.to_string().contains("cache_control"), "{wire}");
}

#[tokio::test]
async fn responses_keep_system_messages_in_position_without_cache_markers() {
    let captured = Arc::new(Mutex::new(Value::Null));
    let sink = captured.clone();
    let app = Router::new().route(
        "/responses",
        post(move |Json(body): Json<Value>| async move {
            *sink.lock().unwrap() = body;
            (StatusCode::BAD_REQUEST, "captured")
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let provider = ResponsesProvider::api_key("synthetic-key", base).unwrap();
    let _ = provider
        .generate(request(), Arc::new(|_| {}), CancellationToken::new())
        .await;
    server.abort();
    let body = captured.lock().unwrap().clone();
    assert_eq!(body["instructions"], "Static prompt.");
    assert_eq!(order(&body["input"]), expected(false));
    assert!(!body.to_string().contains("cache_control"), "{body}");
}
