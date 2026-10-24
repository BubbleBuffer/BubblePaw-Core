//! Streamed replies from the HTTP adapters, read from raw server-sent event bodies.
#[path = "support/eventually.rs"]
mod eventually;
use bubblepaw_core::{
    CancellationToken, Message, ModelRequest, Provider, ProviderError, ProviderErrorKind, Role,
    TextCallback, ToolDefinition,
};
use bubblepaw_runner::provider::{ChatCompletionsProvider, MessagesProvider, ResponsesProvider};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn user_message() -> Message {
    Message {
        id: "fixture-message".into(),
        session_id: "fixture-session".into(),
        role: Role::User,
        origin: None,
        content: "Read the file.".into(),
        tool_calls: Vec::new(),
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: String::new(),
    }
}

fn request(tools: bool) -> ModelRequest {
    ModelRequest {
        model: "mock-model".into(),
        system_prompt: "test".into(),
        messages: vec![user_message()],
        tools: if tools {
            vec![ToolDefinition {
                approval: Default::default(),
                name: "workspace_read".into(),
                description: "read".into(),
                read_only: true,
                input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
            }]
        } else {
            Vec::new()
        },
        max_output_tokens: Some(100),
        ..Default::default()
    }
}

/// Server-sent event data lines for JSON values.
fn events(values: &[Value]) -> String {
    values
        .iter()
        .map(|value| format!("data: {value}\n\n"))
        .collect()
}

/// Chat completion events, ended by the `[DONE]` sentinel.
fn chat_body(values: &[Value]) -> String {
    format!("{}data: [DONE]\n\n", events(values))
}

fn listener() -> (TextCallback, Arc<Mutex<Vec<String>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let sink = calls.clone();
    (
        Arc::new(move |text: &str| sink.lock().unwrap().push(text.to_owned())),
        calls,
    )
}

fn kind_of(error: &anyhow::Error) -> ProviderErrorKind {
    ProviderError::of(error)
        .expect("provider error is classified")
        .kind
        .clone()
}

async fn read_request(socket: &mut TcpStream) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = socket.read(&mut chunk).await.unwrap_or(0);
        if read == 0 {
            return;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if buffer.len() >= end + 4 + length {
                return;
            }
        }
    }
}

/// One-connection server answering with `body` as a close-delimited event stream.
/// With `hold`, the connection stays open after the body.
async fn raw_server(body: String, hold: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let head =
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.write_all(body.as_bytes()).await;
        if hold {
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        let _ = socket.shutdown().await;
    });
    base
}

#[tokio::test]
async fn messages_stream_assembles_the_complete_response() {
    let body = events(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":5,"cache_creation_input_tokens":2,"output_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Plan "}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"the read."}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig+/==abc"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Read"}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"ing."}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"workspace_read","input":{}}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"README.md\"}"}}),
        json!({"type":"content_block_stop","index":2}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}),
        json!({"type":"message_stop"}),
    ]);
    let provider =
        MessagesProvider::api_key("synthetic-key", raw_server(body, false).await).unwrap();
    let (on_text, seen) = listener();
    let response = provider
        .generate(request(true), on_text, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(response.text, "Reading.");
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].id, "toolu_1");
    assert_eq!(
        response.tool_calls[0].arguments,
        json!({"path":"README.md"})
    );
    let usage = response.usage.unwrap();
    assert_eq!(
        (
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_tokens,
            usage.cache_creation_tokens
        ),
        (17, 9, 5, 2)
    );
    let state = response.provider_state.unwrap();
    assert_eq!(state["content"][0]["thinking"], "Plan the read.");
    assert_eq!(state["content"][0]["signature"], "sig+/==abc");
    assert_eq!(state["stop_reason"], "tool_use");
    assert_eq!(seen.lock().unwrap().concat(), "Reading.");
}

#[tokio::test]
async fn messages_stream_batches_text_for_the_listener() {
    let mut values = vec![
        json!({"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
    ];
    values.extend((0..3000).map(|_| {
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"a"}})
    }));
    values.push(json!({"type":"content_block_stop","index":0}));
    values.push(json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3000}}));
    values.push(json!({"type":"message_stop"}));
    let provider =
        MessagesProvider::api_key("synthetic-key", raw_server(events(&values), false).await)
            .unwrap();
    let (on_text, seen) = listener();
    let started = Instant::now();
    let response = provider
        .generate(request(false), on_text, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(response.text.len(), 3000);
    let calls = seen.lock().unwrap();
    assert_eq!(calls.concat().len(), 3000);
    let chars = response.text.chars().count();
    // The public batching contract allows size and 75 ms timer flushes.
    // Slow fixture parsing must not count a permitted timer flush as a failure.
    let allowed = chars as u128 / 512 + started.elapsed().as_millis() / 75 + 2;
    assert!(
        calls.len() as u128 <= allowed,
        "{} listener calls",
        calls.len()
    );
    assert!(
        calls.len() < chars / 16,
        "listener fragments were not batched"
    );
}

#[tokio::test]
async fn messages_drop_after_output_is_fatal_and_keeps_delivered_text() {
    let body = events(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Partial"}}),
    ]);
    let provider =
        MessagesProvider::api_key("synthetic-key", raw_server(body, false).await).unwrap();
    let (on_text, seen) = listener();
    let error = provider
        .generate(request(false), on_text, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("before message_stop"));
    assert!(matches!(kind_of(&error), ProviderErrorKind::Fatal));
    assert_eq!(seen.lock().unwrap().concat(), "Partial");
}

#[tokio::test]
async fn messages_drop_before_output_is_transient() {
    let body = events(&[json!({"type":"message_start","message":{"usage":{"input_tokens":1}}})]);
    let provider =
        MessagesProvider::api_key("synthetic-key", raw_server(body, false).await).unwrap();
    let (on_text, seen) = listener();
    let error = provider
        .generate(request(false), on_text, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(kind_of(&error), ProviderErrorKind::Transient));
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn messages_error_event_is_reported_by_type() {
    let body =
        events(&[json!({"type":"error","error":{"type":"overloaded_error","message":"busy"}})]);
    let provider =
        MessagesProvider::api_key("synthetic-key", raw_server(body, false).await).unwrap();
    let (on_text, _) = listener();
    let error = provider
        .generate(request(false), on_text, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Messages stream reported an error")
    );
    assert!(matches!(kind_of(&error), ProviderErrorKind::Transient));
}

#[tokio::test]
async fn chat_stream_assembles_text_reasoning_and_usage() {
    let body = chat_body(&[
        json!({"choices":[{"delta":{"role":"assistant","reasoning_content":"think "}}]}),
        json!({"choices":[{"delta":{"reasoning_content":"more"}}]}),
        json!({"choices":[{"delta":{"content":"Hel"}}]}),
        json!({"choices":[{"delta":{"content":"lo"},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":20,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":12,"cache_creation_tokens":3},"completion_tokens_details":{"reasoning_tokens":1}}}),
    ]);
    let provider =
        ChatCompletionsProvider::api_key("synthetic-key", raw_server(body, false).await).unwrap();
    let (on_text, seen) = listener();
    let response = provider
        .generate(request(false), on_text, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(response.text, "Hello");
    let usage = response.usage.unwrap();
    assert_eq!(
        (
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_tokens,
            usage.cache_creation_tokens,
            usage.reasoning_tokens
        ),
        (20, 2, 12, 3, Some(1))
    );
    assert_eq!(
        response.provider_state.unwrap()["message_fields"]["reasoning_content"],
        "think more"
    );
    assert_eq!(seen.lock().unwrap().concat(), "Hello");
}

#[tokio::test]
async fn chat_stream_batches_text_for_the_listener() {
    let text = "猫".repeat(3000);
    let mut values: Vec<Value> = (0..3000)
        .map(|_| json!({"choices":[{"index":0,"delta":{"content":"猫"}}]}))
        .collect();
    values.push(json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}));
    let provider = ChatCompletionsProvider::api_key(
        "synthetic-key",
        raw_server(chat_body(&values), false).await,
    )
    .unwrap();
    let (on_text, seen) = listener();
    let started = Instant::now();
    let response = provider
        .generate(request(false), on_text, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(response.text, text);
    let calls = seen.lock().unwrap();
    assert_eq!(calls.concat(), text);
    let chars = text.chars().count();
    // The public batching contract allows size and 75 ms timer flushes.
    // Slow fixture parsing must not count a permitted timer flush as a failure.
    let allowed = chars as u128 / 512 + started.elapsed().as_millis() / 75 + 2;
    assert!(
        calls.len() as u128 <= allowed,
        "{} listener calls",
        calls.len()
    );
    assert!(
        calls.len() < chars / 16,
        "listener fragments were not batched"
    );
}

#[tokio::test]
async fn chat_stream_without_done_is_an_error() {
    let body = events(&[json!({"choices":[{"delta":{"content":"cut"}}]})]);
    let provider =
        ChatCompletionsProvider::api_key("synthetic-key", raw_server(body, false).await).unwrap();
    let (on_text, seen) = listener();
    let error = provider
        .generate(request(false), on_text, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("without [DONE]"));
    assert_eq!(seen.lock().unwrap().concat(), "cut");
}

#[tokio::test]
async fn chat_stream_rejects_tool_calls_while_tools_are_disabled() {
    let body = chat_body(&[
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"workspace_read","arguments":"{}"}}]}}]}),
    ]);
    let provider =
        ChatCompletionsProvider::api_key("synthetic-key", raw_server(body, false).await).unwrap();
    let (on_text, _) = listener();
    let error = provider
        .generate(request(false), on_text, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("tool call with tools disabled"));
}

#[tokio::test]
async fn responses_stream_batches_text_for_the_listener() {
    let text = "b".repeat(2000);
    let mut values: Vec<Value> = (0..2000)
        .map(|_| json!({"type":"response.output_text.delta","delta":"b"}))
        .collect();
    values.push(json!({"type":"response.completed","response":{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}],"usage":{"input_tokens":12,"output_tokens":4}}}));
    let provider =
        ResponsesProvider::api_key("synthetic-key", raw_server(events(&values), false).await)
            .unwrap();
    let (on_text, seen) = listener();
    let started = Instant::now();
    let response = provider
        .generate(request(false), on_text, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(response.text, text);
    let calls = seen.lock().unwrap();
    assert_eq!(calls.concat(), text);
    let chars = text.chars().count();
    // The public batching contract allows size and 75 ms timer flushes.
    // Slow fixture parsing must not count a permitted timer flush as a failure.
    let allowed = chars as u128 / 512 + started.elapsed().as_millis() / 75 + 2;
    assert!(
        calls.len() as u128 <= allowed,
        "{} listener calls",
        calls.len()
    );
    assert!(
        calls.len() < chars / 16,
        "listener fragments were not batched"
    );
}

#[tokio::test]
async fn cancellation_interrupts_an_open_messages_stream() {
    let body = events(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Still"}}),
    ]);
    let provider =
        MessagesProvider::api_key("synthetic-key", raw_server(body, true).await).unwrap();
    let (on_text, texts) = listener();
    let token = CancellationToken::new();
    let cancel = token.clone();
    tokio::spawn(async move {
        eventually::eventually("streamed text", || !texts.lock().unwrap().is_empty()).await;
        cancel.cancel();
    });
    let error = tokio::time::timeout(
        Duration::from_secs(10),
        provider.generate(request(false), on_text, token),
    )
    .await
    .expect("cancellation ends the open stream")
    .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
}

#[tokio::test]
async fn cancellation_interrupts_an_open_chat_stream() {
    let body = events(&[json!({"choices":[{"delta":{"content":"Still"}}]})]);
    let provider =
        ChatCompletionsProvider::api_key("synthetic-key", raw_server(body, true).await).unwrap();
    let (on_text, texts) = listener();
    let token = CancellationToken::new();
    let cancel = token.clone();
    tokio::spawn(async move {
        eventually::eventually("streamed text", || !texts.lock().unwrap().is_empty()).await;
        cancel.cancel();
    });
    let error = tokio::time::timeout(
        Duration::from_secs(10),
        provider.generate(request(false), on_text, token),
    )
    .await
    .expect("cancellation ends the open stream")
    .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
}
