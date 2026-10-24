//! A main turn through `ProviderRegistry::generate` retries one failed unpublished
//! request, and never retries once output reached the listener. A raw TCP mock
//! makes the first reply a clean cut in the middle of a stream.
use bubblepaw_core::{CancellationToken, Message, ModelRequest, Provider, ProviderError, Role};
use bubblepaw_runner::provider::{ChatCompletionsProvider, MessagesProvider, ProviderRegistry};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

fn request() -> ModelRequest {
    ModelRequest {
        model: "fixture".into(),
        messages: vec![Message {
            id: "user".into(),
            session_id: "fixture".into(),
            role: Role::User,
            origin: None,
            content: "Hello".into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: String::new(),
        }],
        ..Default::default()
    }
}

#[derive(Clone, Copy, Debug)]
enum Adapter {
    Chat,
    Messages,
}
impl Adapter {
    fn provider(self, base: String) -> Arc<dyn Provider> {
        match self {
            Adapter::Chat => Arc::new(ChatCompletionsProvider::api_key("synthetic", base).unwrap()),
            Adapter::Messages => Arc::new(MessagesProvider::api_key("synthetic", base).unwrap()),
        }
    }
    fn complete_reply(self) -> Value {
        match self {
            Adapter::Chat => json!({
                "choices": [{"message": {"content": "done"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 1}
            }),
            Adapter::Messages => json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "done"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 3, "output_tokens": 1}
            }),
        }
    }
    /// Stream events for a reply that produced some text and then stops.
    fn partial_events(self) -> Vec<Value> {
        match self {
            Adapter::Chat => {
                vec![json!({"choices": [{"index": 0, "delta": {"content": "Partial"}}]})]
            }
            Adapter::Messages => vec![
                json!({"type": "message_start", "message": {"usage": {"input_tokens": 3, "output_tokens": 1}}}),
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Partial"}}),
            ],
        }
    }
}

/// A complete HTTP/1.1 reply with a length and a closing socket.
fn http_reply(status: &str, headers: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn ok_reply(adapter: Adapter) -> String {
    http_reply(
        "200 OK",
        "content-type: application/json\r\n",
        &adapter.complete_reply().to_string(),
    )
}

/// An event stream with some text and no terminal event; the socket closes after it.
fn cut_reply(adapter: Adapter) -> String {
    let body: String = adapter
        .partial_events()
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect();
    format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}")
}

/// Answers each connection with one raw reply: `first` for the first connection,
/// `later` for every other one. Counts the connections it accepts.
async fn raw_server(
    first: String,
    later: String,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let index = counter.fetch_add(1, Ordering::SeqCst);
            let reply = if index == 0 {
                first.clone()
            } else {
                later.clone()
            };
            tokio::spawn(async move {
                read_request(&mut socket).await;
                let _ = socket.write_all(reply.as_bytes()).await;
            });
        }
    });
    (base, calls, task)
}

/// Reads the request head and its declared body, so the close is clean.
async fn read_request(socket: &mut TcpStream) {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        match socket.read_u8().await {
            Ok(byte) => head.push(byte),
            Err(_) => return,
        }
    }
    let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    let _ = socket.read_exact(&mut body).await;
}

#[tokio::test]
async fn main_turn_retries_one_unavailable_or_rate_limited_reply() {
    for adapter in [Adapter::Chat, Adapter::Messages] {
        for first in [
            http_reply(
                "503 Service Unavailable",
                "content-type: text/plain\r\n",
                "busy",
            ),
            http_reply(
                "429 Too Many Requests",
                "retry-after: 0\r\ncontent-type: text/plain\r\n",
                "slow down",
            ),
        ] {
            let (base, calls, server) = raw_server(first, ok_reply(adapter)).await;
            let registry = ProviderRegistry::new("synthetic", adapter.provider(base));
            let response = tokio::time::timeout(
                Duration::from_secs(10),
                registry.generate(request(), Arc::new(|_| {}), CancellationToken::new()),
            )
            .await
            .expect("the retry must finish")
            .unwrap_or_else(|error| panic!("{adapter:?}: {error:#}"));
            assert_eq!(response.text, "done", "{adapter:?}");
            assert_eq!(calls.load(Ordering::SeqCst), 2, "{adapter:?}");
            server.abort();
        }
    }
}

#[tokio::test]
async fn main_turn_never_retries_after_text_reached_the_listener() {
    for adapter in [Adapter::Chat, Adapter::Messages] {
        let (base, calls, server) = raw_server(cut_reply(adapter), ok_reply(adapter)).await;
        let registry = ProviderRegistry::new("synthetic", adapter.provider(base));
        let error = tokio::time::timeout(
            Duration::from_secs(10),
            registry.generate(request(), Arc::new(|_| {}), CancellationToken::new()),
        )
        .await
        .expect("a cut stream must end the call")
        .unwrap_err();
        assert!(
            ProviderError::of(&error).is_some_and(|typed| typed.published),
            "{adapter:?}: {error:#}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "{adapter:?}");
        server.abort();
    }
}
