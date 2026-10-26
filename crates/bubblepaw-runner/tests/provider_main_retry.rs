//! A main turn through `ProviderRegistry::generate` retries one failed unpublished
//! request, and never retries once output reached the listener. A raw TCP mock
//! makes the first reply a clean cut in the middle of a stream.
use bubblepaw_core::{
    CancellationToken, Message, ModelRequest, Provider, ProviderError, ProviderErrorKind, Role,
};
use bubblepaw_runner::provider::{
    ChatCompletionsProvider, MessagesProvider, ProviderRegistry, ResponsesProvider, RetryPolicy,
};
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
    Responses,
}
impl Adapter {
    fn provider(self, base: String) -> Arc<dyn Provider> {
        match self {
            Adapter::Chat => Arc::new(ChatCompletionsProvider::api_key("synthetic", base).unwrap()),
            Adapter::Messages => Arc::new(MessagesProvider::api_key("synthetic", base).unwrap()),
            Adapter::Responses => Arc::new(ResponsesProvider::api_key("synthetic", base).unwrap()),
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
            Adapter::Responses => json!({
                "type": "response.completed",
                "response": {
                    "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "done"}]}],
                    "usage": {"input_tokens": 3, "output_tokens": 1}
                }
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
            Adapter::Responses => {
                vec![json!({"type": "response.output_text.delta", "delta": "Partial"})]
            }
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
    if matches!(adapter, Adapter::Responses) {
        return http_reply(
            "200 OK",
            "content-type: text/event-stream\r\n",
            &format!("data: {}\n\n", adapter.complete_reply()),
        );
    }
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
    for adapter in [Adapter::Chat, Adapter::Messages, Adapter::Responses] {
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

fn quick_registry(adapter: Adapter, base: String) -> ProviderRegistry {
    ProviderRegistry::new("synthetic", adapter.provider(base)).with_retry_policy(RetryPolicy {
        max_attempts: 3,
        initial_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(1),
    })
}

#[tokio::test]
async fn compatible_main_turn_retries_unstructured_400_then_succeeds() {
    for adapter in [Adapter::Chat, Adapter::Responses] {
        for body in [
            "",
            " \r\n\t",
            "upstream request failed",
            "<html><body>Bad Request</body></html>",
            r#"{"model":"deepseek-v4.1-flash"}"#,
        ] {
            // No Content-Type: classification must use the actual error body.
            let (base, calls, server) =
                raw_server(http_reply("400 Bad Request", "", body), ok_reply(adapter)).await;
            let response = quick_registry(adapter, base)
                .generate(request(), Arc::new(|_| {}), CancellationToken::new())
                .await
                .unwrap_or_else(|error| panic!("{adapter:?} {body:?}: {error:#}"));
            assert_eq!(response.text, "done");
            assert_eq!(calls.load(Ordering::SeqCst), 2, "{adapter:?} {body:?}");
            server.abort();
        }
    }
}

#[tokio::test]
async fn compatible_main_turn_bare_400_exhaustion_keeps_typed_diagnostics_and_retry_count() {
    for adapter in [Adapter::Chat, Adapter::Responses] {
        // Usage exercises the wrappers around the typed HTTP failure too.
        let body = r#"{"model":"deepseek-v4.1-flash","usage":{"prompt_tokens":2,"completion_tokens":1,"input_tokens":2,"output_tokens":1}}"#;
        let reply = http_reply(
            "400 Bad Request",
            "content-type: application/json\r\n",
            body,
        );
        let (base, calls, server) = raw_server(reply.clone(), reply).await;
        let error = quick_registry(adapter, base)
            .generate(request(), Arc::new(|_| {}), CancellationToken::new())
            .await
            .unwrap_err()
            .context("host context");
        let typed = ProviderError::of(&error).unwrap();
        assert_eq!(typed.kind, ProviderErrorKind::Transient);
        assert!(!typed.published);
        assert!(typed.message.contains("HTTP 400"), "{error:#}");
        assert!(typed.message.contains("deepseek-v4.1-flash"), "{error:#}");
        assert!(typed.message.contains("retried 2 times"), "{error:#}");
        assert_eq!(calls.load(Ordering::SeqCst), 3, "{adapter:?}");
        server.abort();
    }
}

#[tokio::test]
async fn compatible_structured_400_is_not_retried_and_keeps_redacted_diagnostics() {
    for adapter in [Adapter::Chat, Adapter::Responses] {
        for (body, expected) in [
            (
                r#"{"error":{"type":"invalid_request_error","code":"invalid_model","message":"invalid model; Bearer echoed-secret","api_key":"nested-secret"}}"#,
                ProviderErrorKind::Fatal,
            ),
            (
                r#"{"error":{"code":"context_length_exceeded","message":"too much context; api_key=echoed-secret"}}"#,
                ProviderErrorKind::Context,
            ),
        ] {
            let (base, calls, server) = raw_server(
                http_reply(
                    "400 Bad Request",
                    "content-type: application/json\r\n",
                    body,
                ),
                ok_reply(adapter),
            )
            .await;
            let error = quick_registry(adapter, base)
                .generate(request(), Arc::new(|_| {}), CancellationToken::new())
                .await
                .unwrap_err();
            let typed = ProviderError::of(&error).unwrap();
            assert_eq!(typed.kind, expected, "{adapter:?}: {error:#}");
            assert!(typed.message.contains("HTTP 400"));
            assert!(typed.message.contains("code="));
            assert!(typed.message.contains("[redacted]"));
            for secret in ["echoed-secret", "nested-secret"] {
                assert!(!format!("{error:#}").contains(secret), "{error:#}");
            }
            assert_eq!(calls.load(Ordering::SeqCst), 1, "{adapter:?}");
            server.abort();
        }
    }
}

#[tokio::test]
async fn messages_bare_400_remains_fatal_without_retry() {
    let (base, calls, server) = raw_server(
        http_reply("400 Bad Request", "", r#"{"model":"fixture"}"#),
        ok_reply(Adapter::Messages),
    )
    .await;
    let error = quick_registry(Adapter::Messages, base)
        .generate(request(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(
        ProviderError::of(&error).unwrap().kind,
        ProviderErrorKind::Fatal
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn compatible_bare_400_after_published_continuation_is_never_retried() {
    for adapter in [Adapter::Chat, Adapter::Responses] {
        let first = match adapter {
            Adapter::Chat => http_reply(
                "200 OK", "content-type: application/json\r\n",
                &json!({"choices":[{"message":{"content":"Partial"},"finish_reason":"length"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}).to_string(),
            ),
            Adapter::Responses => http_reply(
                "200 OK", "content-type: text/event-stream\r\n",
                &format!("data: {}\n\n", json!({"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[{"type":"message","content":[{"type":"output_text","text":"Partial"}]}],"usage":{"input_tokens":3,"output_tokens":1}}})),
            ),
            Adapter::Messages => unreachable!(),
        };
        let (base, calls, server) = raw_server(
            first,
            http_reply("400 Bad Request", "", r#"{"model":"fixture"}"#),
        )
        .await;
        let text = Arc::new(std::sync::Mutex::new(String::new()));
        let sink = text.clone();
        let error = quick_registry(adapter, base)
            .generate(
                request(),
                Arc::new(move |delta| sink.lock().unwrap().push_str(delta)),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        let typed = ProviderError::of(&error).unwrap();
        assert_eq!(typed.kind, ProviderErrorKind::Transient);
        assert!(typed.published, "{adapter:?}: {error:#}");
        assert_eq!(*text.lock().unwrap(), "Partial");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "{adapter:?}");
        server.abort();
    }
}

#[tokio::test]
async fn bare_400_warns_once_per_retry_with_provider_status_and_attempt() {
    #[derive(Clone)]
    struct Logs(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Logs {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let capture = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || Logs(capture.clone()))
        .finish();
    // This binary has one subscriber. A persistent global dispatch keeps
    // callsite filtering stable while other provider fixtures run concurrently.
    // Its unique route lets us count only warnings from this request.
    tracing::subscriber::set_global_default(subscriber).unwrap();
    let reply = http_reply("400 Bad Request", "", r#"{"model":"fixture"}"#);
    let (base, calls, server) = raw_server(reply.clone(), reply).await;
    let registry = ProviderRegistry::new("warning-fixture", Adapter::Chat.provider(base))
        .with_retry_policy(RetryPolicy {
            max_attempts: 3,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
        });
    let error = registry
        .generate(request(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("retried 2 times"));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    let warnings: Vec<_> = logs
        .lines()
        .filter(|line| {
            line.contains("Retrying unstructured HTTP 400")
                && line.contains("provider=\"warning-fixture\"")
        })
        .collect();
    assert_eq!(warnings.len(), 2, "{logs}");
    for (index, warning) in warnings.iter().enumerate() {
        assert!(warning.contains("WARN"), "{warning}");
        assert!(
            warning.contains("provider=\"warning-fixture\""),
            "{warning}"
        );
        assert!(warning.contains("status=400"), "{warning}");
        assert!(
            warning.contains(&format!("attempt={}", index + 1)),
            "{warning}"
        );
    }
    server.abort();
}

#[tokio::test]
async fn compatible_decision_route_retries_bare_400_using_the_registry_budget() {
    use bubblepaw_core::{DecisionProvider, DecisionRequest, Question, QuestionKind};
    use bubblepaw_runner::provider::OpenAiDecisions;

    for succeeds in [true, false] {
        let rejection = http_reply("400 Bad Request", "", r#"{"model":"fixture"}"#);
        let later = if succeeds {
            http_reply(
                "200 OK",
                "content-type: application/json\r\n",
                r#"{"answers":[{"name":"valid","type":"predicate","probability":0.75}],"usage":{"input_tokens":3,"output_tokens":1}}"#,
            )
        } else {
            rejection.clone()
        };
        let (base, calls, server) = raw_server(rejection, later).await;
        let mut registry = quick_registry(Adapter::Chat, base.clone());
        registry
            .insert_decision(
                "decider",
                Arc::new(OpenAiDecisions::new("synthetic", base).unwrap()),
            )
            .unwrap();
        let result = registry
            .decide(
                DecisionRequest {
                    model: "decider/fixture".into(),
                    input: "Is this valid?".into(),
                    questions: vec![Question {
                        name: "valid".into(),
                        instructions: "Decide validity.".into(),
                        kind: QuestionKind::Predicate,
                    }],
                    session_id: Some("fixture".into()),
                    reasoning_effort: Default::default(),
                },
                CancellationToken::new(),
            )
            .await;
        if succeeds {
            assert_eq!(result.unwrap().answers.len(), 1);
        } else {
            let error = result.unwrap_err();
            let typed = ProviderError::of(&error).unwrap();
            assert_eq!(typed.kind, ProviderErrorKind::Transient);
            assert!(typed.message.contains("retried 2 times"), "{error:#}");
        }
        assert_eq!(calls.load(Ordering::SeqCst), if succeeds { 2 } else { 3 });
        server.abort();
    }
}
