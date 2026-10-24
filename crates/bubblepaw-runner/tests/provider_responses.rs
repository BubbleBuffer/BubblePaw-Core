#[path = "support/eventually.rs"]
mod eventually;
#[path = "support/http_server.rs"]
mod http_server;
use axum::{
    Json, Router,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use bubblepaw_core::{
    CancellationToken, Message, ModelRequest, Provider, ProviderError, ProviderErrorKind,
    ReasoningEffort, Role, ToolDefinition, provider_error_retryable,
};
use bubblepaw_runner::provider::{
    ProviderRegistry, ResponsesProvider, RetryPolicy, decision_error_usage, provider_retry_after,
};
use http_server::serve;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

fn request() -> ModelRequest {
    ModelRequest {
        model: "mock-model".into(),
        system_prompt: "test".into(),
        messages: Vec::new(),
        tools: vec![ToolDefinition {
            approval: Default::default(),
            name: "workspace_read".into(),
            description: "read".into(),
            read_only: true,
            input_schema: json!({"type":"object","properties":{"path":{"type":"string"},"start_line":{"type":"integer"}},"required":["path"],"additionalProperties":false}),
        }],
        max_output_tokens: Some(100),
        ..Default::default()
    }
}
fn sse() -> String {
    let call = json!({"type":"function_call","call_id":"call-1","name":"workspace_read","arguments":"{\"path\":\"README.md\"}"});
    [json!({"type":"response.output_text.delta","delta":"Reading."}),json!({"type":"response.output_item.done","output_index":0,"item":call}),json!({"type":"response.completed","response":{"output":[{"type":"reasoning","encrypted_content":"synthetic-encrypted"},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Reading."}]},call],"usage":{"input_tokens":12,"output_tokens":4}}})].iter().map(|event|format!("data: {event}\r\n\r\n")).collect()
}

#[tokio::test]
async fn malformed_tool_calls_are_metered_and_do_not_publish_text() {
    let valid = json!({"type":"function_call","call_id":"call-1","name":"workspace_read","arguments":"{\"path\":\"README.md\"}"});
    for malformed in [
        json!({"type":"function_call","call_id":"call-2","name":"workspace_read","arguments":"{bad-json"}),
        json!({"type":"function_call","call_id":"call-2","name":"workspace_read"}),
        json!({"type":"function_call","call_id":"call-2","name":"workspace_read","arguments":42}),
        json!({"type":"function_call","call_id":"call-2","name":"workspace_read","arguments":null}),
        valid.clone(),
        json!({"type":"function_call","name":"workspace_read","arguments":"{}"}),
        json!({"type":"function_call","call_id":"call-2","arguments":"{}"}),
        json!({"type":"function_call","call_id":"","name":"workspace_read","arguments":"{}"}),
        json!({"type":"function_call","call_id":"call-2","name":"","arguments":"{}"}),
        json!({"type":"function_call","call_id":"call-2","name":"not-offered","arguments":"{}"}),
        json!({"type":"function_call","call_id":"call-2","name":"workspace_read","arguments":"true"}),
        json!({"type":"function_call","call_id":"call-2","name":"workspace_read","arguments":"[]"}),
        json!({"type":"function_call","call_id":"call-2","name":"workspace_read","arguments":"null"}),
    ] {
        for reported in [false, true] {
            let output = json!([
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"Must not publish."}]},
                valid.clone(),
                malformed.clone(),
            ]);
            let mut response = json!({"status":"completed","output":output});
            if reported {
                response["usage"] = json!({"input_tokens":12,"output_tokens":4});
            }
            let mut events: Vec<Value> = output.as_array().unwrap().iter().enumerate()
                .map(|(index, item)| json!({"type":"response.output_item.done","output_index":index,"item":item}))
                .collect();
            events.push(json!({"type":"response.completed","response":response}));
            let body: String = events
                .iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect();
            let calls = Arc::new(AtomicUsize::new(0));
            let counter = calls.clone();
            let app = Router::new().route(
                "/responses",
                post(move || {
                    let counter = counter.clone();
                    let body = body.clone();
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        ([("content-type", "text/event-stream")], body)
                    }
                }),
            );
            let (base, server) = serve(app).await;
            let provider = ResponsesProvider::api_key("synthetic-key", base).unwrap();
            let error = provider
                .generate(
                    request(),
                    Arc::new(|_| panic!("invalid response published text")),
                    CancellationToken::new(),
                )
                .await
                .unwrap_err();
            assert_eq!(
                ProviderError::of(&error).unwrap().kind,
                ProviderErrorKind::Fatal
            );
            if reported {
                let usage = decision_error_usage(&error)
                    .expect("reported usage must survive malformed call rejection");
                assert_eq!((usage.input_tokens, usage.output_tokens), (12, 4));
                assert_eq!(usage.cost_usd, None);
            } else {
                assert!(decision_error_usage(&error).is_none());
            }
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            server.abort();
        }
    }
}

#[tokio::test]
async fn length_stop_drops_scalar_and_array_calls_before_continuing() {
    let observed = Arc::new(Mutex::new(Vec::<Value>::new()));
    let requests = observed.clone();
    let app = Router::new().route("/responses", post(move |Json(body): Json<Value>| {
        let requests = requests.clone();
        async move {
            let first = {
                let mut requests = requests.lock().unwrap();
                requests.push(body);
                requests.len() == 1
            };
            let response = if first {
                json!({"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[
                    {"type":"message","role":"assistant","content":[{"type":"output_text","text":"Part "}]},
                    {"type":"function_call","call_id":"scalar","name":"workspace_read","arguments":"true"},
                    {"type":"function_call","call_id":"array","name":"workspace_read","arguments":"[]"}
                ]})
            } else {
                json!({"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done."}]}]})
            };
            let kind = if first { "response.incomplete" } else { "response.completed" };
            ([("content-type", "text/event-stream")], format!("data: {}\n\n", json!({"type":kind,"response":response})))
        }
    }));
    let (base, server) = serve(app).await;
    let response = ResponsesProvider::api_key("synthetic-key", base)
        .unwrap()
        .generate(request(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(response.text, "Part done.");
    assert!(response.tool_calls.is_empty());
    let requests = observed.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]["input"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["type"] != "function_call")
    );
    server.abort();
}

#[tokio::test]
async fn transient_retry_keeps_native_optional_tools_and_normalizes_calls() {
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let calls2 = calls.clone();
    let observed2 = observed.clone();
    let app = Router::new().route(
        "/responses",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let calls = calls2.clone();
            let observed = observed2.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer synthetic-key");
                observed.lock().unwrap().push(body);
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        [("retry-after", "0")],
                        "retry",
                    )
                        .into_response()
                } else {
                    ([("content-type", "text/event-stream")], sse()).into_response()
                }
            }
        }),
    );
    let (base, server) = serve(app).await;
    let provider = ResponsesProvider::api_key("synthetic-key", base).unwrap();
    // The registry owns transport retries for every adapter.
    let mut registry = ProviderRegistry::new("responses", Arc::new(provider));
    registry.set_retry_policy(RetryPolicy {
        max_attempts: 3,
        initial_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(5),
    });
    let text = Arc::new(Mutex::new(String::new()));
    let sink = text.clone();
    let result = registry
        .generate(
            request(),
            Arc::new(move |delta| sink.lock().unwrap().push_str(delta)),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(result.text, "Reading.");
    assert_eq!(*text.lock().unwrap(), "Reading.");
    assert_eq!(result.tool_calls[0].arguments["path"], "README.md");
    assert_eq!(result.usage.unwrap().input_tokens, 12);
    assert_eq!(
        result.provider_state.unwrap()["items"][0]["encrypted_content"],
        "synthetic-encrypted"
    );
    let observed = observed.lock().unwrap();
    assert_eq!(observed[0]["tools"][0]["strict"], false);
    assert_eq!(
        observed[0]["tools"][0]["parameters"]["required"],
        json!(["path"])
    );
    server.abort();
}
#[tokio::test]
async fn permanent_error_does_not_retry_and_backoff_is_cancelable() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let app = Router::new().route(
        "/responses",
        post(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                StatusCode::UNAUTHORIZED
            }
        }),
    );
    let (base, server) = serve(app).await;
    let provider = ResponsesProvider::api_key("synthetic-key", base).unwrap();
    assert!(
        provider
            .generate(request(), Arc::new(|_| {}), CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.abort();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let app = Router::new().route(
        "/responses",
        post(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "30")])
            }
        }),
    );
    let (base, server) = serve(app).await;
    let provider = ResponsesProvider::api_key("synthetic-key", base).unwrap();
    let cancellation = CancellationToken::new();
    let token = cancellation.clone();
    let task =
        tokio::spawn(async move { provider.generate(request(), Arc::new(|_| {}), token).await });
    eventually::eventually("the 429 reply", || hits.load(Ordering::SeqCst) > 0).await;
    cancellation.cancel();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    server.abort();
}
#[tokio::test]
async fn codex_auth_is_reread_and_output_limit_is_not_sent() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("auth.json");
    std::fs::write(
        &path,
        r#"{"tokens":{"access_token":"old-synthetic","account_id":"synthetic-account"}}"#,
    )
    .unwrap();
    let app = Router::new().route(
        "/responses",
        post(|headers: HeaderMap, Json(body): Json<Value>| async move {
            assert_eq!(headers["authorization"], "Bearer new-synthetic");
            assert_eq!(headers["chatgpt-account-id"], "synthetic-account");
            assert!(body.get("max_output_tokens").is_none());
            ([("content-type", "text/event-stream")], sse()).into_response()
        }),
    );
    let (base, server) = serve(app).await;
    let provider = ResponsesProvider::codex_auth(&path, base).unwrap();
    std::fs::write(
        &path,
        r#"{"tokens":{"access_token":"new-synthetic","account_id":"synthetic-account"}}"#,
    )
    .unwrap();
    let expected = std::fs::read(&path).unwrap();
    provider
        .generate(request(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), expected);
    server.abort();
}

#[tokio::test]
async fn complete_reasoning_and_calls_are_replayed_intact_in_next_round() {
    let observed = Arc::new(Mutex::new(Vec::<Value>::new()));
    let sink = observed.clone();
    let app = Router::new().route(
        "/responses",
        post(move |Json(body): Json<Value>| {
            let sink = sink.clone();
            async move {
                sink.lock().unwrap().push(body);
                ([("content-type", "text/event-stream")], sse())
            }
        }),
    );
    let (base, server) = serve(app).await;
    let provider = ResponsesProvider::api_key("synthetic", base).unwrap();
    let response = provider
        .generate(request(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    let state = response.provider_state.clone().unwrap();
    let mut second = request();
    second.messages = vec![
        Message {
            id: "assistant".into(),
            session_id: "s".into(),
            role: Role::Assistant,
            content: response.text,
            tool_calls: response.tool_calls,
            origin: None,
            provider_state: response.provider_state,
            tool_call_id: None,
            is_error: false,
            created_at: String::new(),
        },
        Message {
            id: "tool".into(),
            session_id: "s".into(),
            role: Role::Tool,
            content: "contents".into(),
            tool_calls: vec![],
            origin: None,
            provider_state: None,
            tool_call_id: Some("call-1".into()),
            is_error: false,
            created_at: String::new(),
        },
    ];
    provider
        .generate(second, Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    let observed = observed.lock().unwrap();
    let input = observed[1]["input"].as_array().unwrap();
    assert_eq!(input[0], state["items"][0]);
    assert_eq!(
        input[1],
        json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"Reading."}]})
    );
    assert_eq!(
        input[2],
        json!({"type":"function_call","call_id":"call-1","name":"workspace_read","arguments":"{\"path\":\"README.md\"}"})
    );
    assert!(state["items"][1]["content"][0].get("text").is_none());
    assert!(state["items"][2].get("arguments").is_none());
    assert_eq!(
        input[3],
        json!({"type":"function_call_output","call_id":"call-1","output":"contents"})
    );
    server.abort();
}

#[tokio::test]
async fn slim_and_legacy_state_replay_byte_exact_mixed_output_over_http() {
    let items = json!([
        {"type":"reasoning","id":"r1","encrypted_content":"opaque+/==","summary":[]},
        {"type":"message","id":"m1","role":"assistant","content":[{"type":"output_text","text":"α","annotations":[]}]},
        {"type":"function_call","id":"f1","call_id":"call-1","name":"workspace_read","arguments":"{ \"start_line\" : 2, \"path\" : \"README.md\" }"},
        {"type":"message","id":"m2","role":"assistant","content":[{"type":"output_text","text":"β"},{"type":"output_text","text":"γ"}]}
    ]);
    let observed = Arc::new(Mutex::new(Vec::new()));
    let sink = observed.clone();
    let output = items.clone();
    let app = Router::new().route(
        "/responses",
        post(move |Json(body): Json<Value>| {
            let sink = sink.clone();
            let output = output.clone();
            async move {
                sink.lock().unwrap().push(body);
                (
                    [("content-type", "text/event-stream")],
                    format!(
                        "data: {}\n\n",
                        json!({"type":"response.completed","response":{"output":output}})
                    ),
                )
            }
        }),
    );
    let (base, server) = serve(app).await;
    let provider = ResponsesProvider::api_key("synthetic", base).unwrap();
    let response = provider
        .generate(request(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(response.text, "αβγ");
    let assistant = Message {
        id: "assistant".into(),
        session_id: "session".into(),
        role: Role::Assistant,
        origin: None,
        content: response.text,
        tool_calls: response.tool_calls,
        tool_call_id: None,
        provider_state: response.provider_state,
        is_error: false,
        created_at: String::new(),
    };
    // Exercise the durable JSON representation, rather than only live object reuse.
    let mut restored: Message =
        serde_json::from_slice(&serde_json::to_vec(&assistant).unwrap()).unwrap();
    assert!(
        restored.provider_state.as_ref().unwrap()["items"][2]
            .get("arguments")
            .is_none()
    );
    for legacy in [false, true] {
        if legacy {
            restored.provider_state = Some(json!({"adapter":"responses","items":items}));
        }
        let saved = restored.provider_state.clone();
        let mut next = request();
        next.messages.push(restored.clone());
        provider
            .generate(next, Arc::new(|_| {}), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(restored.provider_state, saved);
    }
    let observed = observed.lock().unwrap();
    assert_eq!(
        serde_json::to_vec(&observed[1]).unwrap(),
        serde_json::to_vec(&observed[2]).unwrap(),
        "Complete request bodies must match for slim and legacy replay state"
    );
    for body in &observed[1..] {
        assert_eq!(
            serde_json::to_vec(&body["input"]).unwrap(),
            serde_json::to_vec(&items).unwrap()
        );
    }
    server.abort();
}

#[tokio::test]
async fn published_stream_failure_does_not_retry() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let app = Router::new().route(
        "/responses",
        post(move || {
            let count = count.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                (
                    [("content-type", "text/event-stream")],
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"seen\"}\n\n",
                )
            }
        }),
    );
    let (base, server) = serve(app).await;
    let provider = ResponsesProvider::api_key("synthetic", base).unwrap();
    let text = Arc::new(Mutex::new(String::new()));
    let sink = text.clone();
    let error = provider
        .generate(
            request(),
            Arc::new(move |s| sink.lock().unwrap().push_str(s)),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("without response.completed"));
    assert_eq!(*text.lock().unwrap(), "seen");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn cancellation_interrupts_an_open_sse_stream() {
    let app = Router::new().route(
        "/responses",
        post(|| async {
            let chunks = futures_util::stream::once(async {
                Ok::<_, std::io::Error>(
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"seen\"}\n\n",
                )
            });
            use futures_util::StreamExt;
            let stream = chunks.chain(futures_util::stream::pending());
            (
                [("content-type", "text/event-stream")],
                axum::body::Body::from_stream(stream),
            )
        }),
    );
    let (base, server) = serve(app).await;
    let provider = ResponsesProvider::api_key("synthetic", base).unwrap();
    let cancellation = CancellationToken::new();
    let sink = cancellation.clone();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        provider.generate(request(), Arc::new(move |_| sink.cancel()), cancellation),
    )
    .await
    .unwrap();
    assert!(result.unwrap_err().to_string().contains("cancelled"));
    server.abort();
}
async fn failure(
    status: StatusCode,
    headers: Vec<(&'static str, &'static str)>,
    body: &'static str,
) -> anyhow::Error {
    let app = Router::new().route(
        "/responses",
        post(move || {
            let headers = headers.clone();
            async move {
                let mut map = HeaderMap::new();
                for (name, value) in headers {
                    map.insert(name, value.parse().unwrap());
                }
                (status, map, body).into_response()
            }
        }),
    );
    let (base, server) = serve(app).await;
    let provider = ResponsesProvider::api_key("synthetic-key", base).unwrap();
    let error = provider
        .generate(request(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap_err();
    server.abort();
    error
}
#[tokio::test]
async fn http_failures_map_to_provider_error_kinds() {
    use bubblepaw_core::{ProviderError, ProviderErrorKind};
    let kind = |error: &anyhow::Error| ProviderError::of(error).expect("typed").kind.clone();
    let error = failure(StatusCode::UNAUTHORIZED, vec![], "").await;
    assert_eq!(kind(&error), ProviderErrorKind::Auth);
    assert_eq!(error.to_string(), "Responses HTTP 401");
    let error = failure(StatusCode::FORBIDDEN, vec![], "").await;
    assert_eq!(kind(&error), ProviderErrorKind::Auth);
    let before = chrono::Utc::now();
    let error = failure(
        StatusCode::TOO_MANY_REQUESTS,
        vec![("retry-after", "120")],
        "",
    )
    .await;
    let ProviderErrorKind::RateLimit {
        resets_at: Some(resets_at),
    } = kind(&error)
    else {
        panic!("expected a rate limit with a reset: {error:?}");
    };
    let resets_at = chrono::DateTime::parse_from_rfc3339(&resets_at).unwrap();
    assert!(resets_at >= before + chrono::Duration::seconds(119));
    assert!(resets_at <= chrono::Utc::now() + chrono::Duration::seconds(121));
    assert_eq!(error.to_string(), "Responses HTTP 429");
    let error = failure(StatusCode::TOO_MANY_REQUESTS, vec![], "").await;
    assert_eq!(
        kind(&error),
        ProviderErrorKind::RateLimit { resets_at: None }
    );
    let error = failure(
        StatusCode::BAD_REQUEST,
        vec![("content-type", "application/json")],
        r#"{"error":{"message":"Your input exceeds the context window of this model.","type":"invalid_request_error","code":"context_length_exceeded"}}"#,
    )
    .await;
    assert_eq!(kind(&error), ProviderErrorKind::Context);
    let diagnostic = error.to_string();
    assert!(diagnostic.starts_with("Responses HTTP 400: type=invalid_request_error; code=context_length_exceeded; message=Your input exceeds the context window of this model."));
    assert!(diagnostic.contains("body="));
    let error = failure(
        StatusCode::BAD_REQUEST,
        vec![],
        r#"{"error":{"code":"invalid_value"}}"#,
    )
    .await;
    assert_eq!(kind(&error), ProviderErrorKind::Fatal);
    let error = failure(StatusCode::INTERNAL_SERVER_ERROR, vec![], "").await;
    assert_eq!(kind(&error), ProviderErrorKind::Transient);
    assert_eq!(error.to_string(), "Responses HTTP 500");
    let error = failure(StatusCode::NOT_FOUND, vec![], "").await;
    assert_eq!(kind(&error), ProviderErrorKind::Fatal);
}
#[tokio::test]
async fn connection_failure_is_transient_and_cached_tokens_are_parsed() {
    use bubblepaw_core::{ProviderError, ProviderErrorKind};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let provider = ResponsesProvider::api_key("synthetic-key", base).unwrap();
    let error = provider
        .generate(request(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(
        ProviderError::of(&error).unwrap().kind,
        ProviderErrorKind::Transient
    );
    let capabilities = provider.capabilities("gpt-x");
    assert_eq!(capabilities.cache, bubblepaw_core::CacheMode::Prefix);
    assert!(!capabilities.stateful);
    let events: String = [
        json!({"type":"response.output_text.delta","delta":"Hi"}),
        json!({"type":"response.completed","response":{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Hi"}]}],"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":64,"cache_creation_tokens":7},"output_tokens":12,"output_tokens_details":{"reasoning_tokens":10}}}}),
    ]
    .iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect();
    let app = Router::new().route(
        "/responses",
        post(move || {
            let events = events.clone();
            async move { ([("content-type", "text/event-stream")], events).into_response() }
        }),
    );
    let (base, server) = serve(app).await;
    let provider = ResponsesProvider::api_key("synthetic-key", base).unwrap();
    let usage = provider
        .generate(request(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap()
        .usage
        .unwrap();
    assert_eq!(usage.input_tokens, 100);
    assert_eq!(usage.cache_read_tokens, 64);
    assert_eq!(usage.cache_creation_tokens, 7);
    assert_eq!(usage.output_tokens, 12);
    assert_eq!(usage.reasoning_tokens, Some(10));
    server.abort();
}

#[tokio::test]
async fn responses_to_an_opencode_route_sends_the_session_header() {
    let observed = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    let seen = observed.clone();
    let app = Router::new().route(
        "/zen/go/v1/responses",
        post(move |headers: HeaderMap| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push(
                    headers
                        .get("x-opencode-session")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned),
                );
                let event = json!({"type":"response.completed","response":{"output":[{"type":"message","content":[{"type":"output_text","text":"OK"}]}],"usage":{"input_tokens":4,"output_tokens":1}}});
                ([("content-type", "text/event-stream")], format!("data: {event}\n\n"))
            }
        }),
    );
    let (base, server) = serve(app).await;
    let provider = ResponsesProvider::api_key("synthetic-key", format!("{base}/zen/go/v1"))
        .unwrap()
        .with_opencode_route();
    let mut explicit = request();
    explicit.session_id = "explicit-session".into();
    provider
        .generate(explicit, Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    provider
        .generate(request(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    server.abort();
    let seen = observed.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].as_deref(), Some("explicit-session"));
    let fallback = seen[1]
        .as_deref()
        .expect("session header on fallback request");
    assert!(uuid::Uuid::parse_str(fallback).is_ok());
}

#[tokio::test]
async fn responses_effort_request_overrides_config_and_decisions_default_low() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let observations = observed.clone();
    let app = Router::new().route("/responses",post(move |Json(body):Json<Value>| {
        let observations = observations.clone();
        async move {
            observations.lock().unwrap().push(body);
            let event = json!({"type":"response.completed","response":{"output":[{"type":"message","content":[{"type":"output_text","text":"OK"}]}],"usage":{"input_tokens":4,"output_tokens":1}}});
            ([("content-type","text/event-stream")],format!("data: {event}\n\n"))
        }
    }));
    let (base, server) = serve(app).await;
    let provider = ResponsesProvider::api_key("synthetic-key", base)
        .unwrap()
        .with_reasoning_effort("medium");
    for (decision, effort) in [
        (false, None),
        (false, Some(ReasoningEffort::High)),
        (true, None),
        (true, Some(ReasoningEffort::Max)),
    ] {
        let mut req = request();
        req.reasoning_effort = effort;
        let response = if decision {
            provider
                .generate_decision(req, Arc::new(|_| {}), CancellationToken::new())
                .await
                .unwrap()
        } else {
            provider
                .generate(req, Arc::new(|_| {}), CancellationToken::new())
                .await
                .unwrap()
        };
        let usage = response.usage.unwrap();
        assert_eq!(
            (usage.cache_read_tokens, usage.cache_creation_tokens),
            (0, 0)
        );
    }
    for (body, effort) in observed
        .lock()
        .unwrap()
        .iter()
        .zip(["medium", "high", "low", "max"])
    {
        assert_eq!(body["reasoning"]["effort"], effort);
    }
    server.abort();
    for effort in ["none", "disabled", "unknown", ""] {
        let invalid = ResponsesProvider::api_key("synthetic-key", "http://127.0.0.1:9")
            .unwrap()
            .with_reasoning_effort(effort);
        let error = invalid
            .generate(request(), Arc::new(|_| {}), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Unsupported reasoning effort"));
        assert_eq!(
            bubblepaw_core::ProviderError::of(&error).unwrap().kind,
            bubblepaw_core::ProviderErrorKind::Fatal
        );
    }
}

#[tokio::test]
async fn parallel_streamed_function_calls_round_trip_both_results() {
    let items = json!([
        {"type":"reasoning","encrypted_content":"synthetic-parallel-state"},
        {"type":"function_call","id":"item-a","call_id":"call-a","name":"workspace_read","arguments":"{\"path\":\"a\"}"},
        {"type":"function_call","id":"item-b","call_id":"call-b","name":"workspace_read","arguments":"{\"path\":\"b\",\"start_line\":2}"}
    ]);
    let events: String = [
        json!({"type":"response.output_item.done","output_index":2,"item":items[2]}),
        json!({"type":"response.output_item.done","output_index":0,"item":items[0]}),
        json!({"type":"response.output_item.done","output_index":1,"item":items[1]}),
        json!({"type":"response.completed","response":{"output":[],"usage":{"input_tokens":10,"output_tokens":5,"input_tokens_details":{"cached_tokens":3},"cache_creation_input_tokens":2}}}),
    ].iter().map(|event| format!("data: {event}\n\n")).collect();
    let observed = Arc::new(Mutex::new(Vec::<Value>::new()));
    let sink = observed.clone();
    let app = Router::new().route("/responses", post(move |Json(body): Json<Value>| {
        let sink = sink.clone();
        let events = events.clone();
        async move {
            let first = {
                let mut requests = sink.lock().unwrap();
                requests.push(body);
                requests.len() == 1
            };
            let events = if first { events } else {
                format!("data: {}\n\n", json!({"type":"response.completed","response":{"output":[{"type":"message","content":[{"type":"output_text","text":"Done."}]}]}}))
            };
            ([( "content-type", "text/event-stream" )], events)
        }
    }));
    let (base, server) = serve(app).await;
    let provider = ResponsesProvider::api_key("synthetic", base).unwrap();
    let first = provider
        .generate(request(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(first.tool_calls.len(), 2);
    assert_eq!(first.tool_calls[0].id, "call-a");
    assert_eq!(first.tool_calls[1].id, "call-b");
    assert_eq!(first.tool_calls[0].arguments, json!({"path":"a"}));
    assert_eq!(
        first.tool_calls[1].arguments,
        json!({"path":"b","start_line":2})
    );
    let usage = first.usage.as_ref().unwrap();
    assert_eq!(
        (usage.cache_read_tokens, usage.cache_creation_tokens),
        (3, 2)
    );
    assert_eq!(usage.reasoning_tokens, None);
    let assistant = Message {
        id: "assistant".into(),
        session_id: "session".into(),
        role: Role::Assistant,
        origin: None,
        content: first.text,
        tool_calls: first.tool_calls,
        tool_call_id: None,
        provider_state: first.provider_state,
        is_error: false,
        created_at: String::new(),
    };
    let restored: Message =
        serde_json::from_slice(&serde_json::to_vec(&assistant).unwrap()).unwrap();
    let mut next = request();
    next.messages.push(restored);
    for (id, content) in [("call-a", "result-a"), ("call-b", "result-b")] {
        next.messages.push(Message {
            id: id.into(),
            session_id: "session".into(),
            role: Role::Tool,
            origin: None,
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: Some(id.into()),
            provider_state: None,
            is_error: false,
            created_at: String::new(),
        });
    }
    let second = provider
        .generate(next, Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(second.text, "Done.");
    let observed = observed.lock().unwrap();
    assert_eq!(observed.len(), 2);
    assert_eq!(
        &observed[1]["input"].as_array().unwrap()[..3],
        items.as_array().unwrap()
    );
    assert_eq!(
        observed[1]["input"][3],
        json!({"type":"function_call_output","call_id":"call-a","output":"result-a"})
    );
    assert_eq!(
        observed[1]["input"][4],
        json!({"type":"function_call_output","call_id":"call-b","output":"result-b"})
    );
    server.abort();
}

#[tokio::test]
async fn continuation_sums_reported_reasoning_usage_without_estimation() {
    for second_reasoning in [None, Some(0), Some(7)] {
        let observed = Arc::new(AtomicUsize::new(0));
        let counter = observed.clone();
        let app = Router::new().route("/responses", post(move || {
            let counter = counter.clone();
            async move {
                let first = counter.fetch_add(1, Ordering::SeqCst) == 0;
                let mut usage = json!({"input_tokens":10,"output_tokens":15,"input_tokens_details":{"cached_tokens":3,"cache_creation_tokens":2}});
                if let Some(tokens) = if first { Some(5) } else { second_reasoning } {
                    usage["output_tokens_details"] = json!({"reasoning_tokens":tokens});
                }
                let mut response = json!({"output":[{"type":"message","content":[{"type":"output_text","text":if first {"Part "} else {"two."}}]}],"usage":usage});
                if first {
                    response["status"] = json!("incomplete");
                    response["incomplete_details"] = json!({"reason":"max_output_tokens"});
                }
                let event = json!({"type":if first {"response.incomplete"} else {"response.completed"},"response":response});
                ([( "content-type", "text/event-stream" )], format!("data: {event}\n\n"))
            }
        }));
        let (base, server) = serve(app).await;
        let provider = ResponsesProvider::api_key("synthetic", base).unwrap();
        let response = provider
            .generate(request(), Arc::new(|_| {}), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(response.text, "Part two.");
        assert_eq!(observed.load(Ordering::SeqCst), 2);
        let usage = response.usage.unwrap();
        assert_eq!(
            (
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_tokens,
                usage.cache_creation_tokens
            ),
            (20, 30, 6, 4)
        );
        assert_eq!(
            usage.reasoning_tokens,
            Some(5 + second_reasoning.unwrap_or(0))
        );
        server.abort();
    }
}

#[tokio::test]
async fn precancelled_responses_call_never_reaches_transport() {
    let count = Arc::new(AtomicUsize::new(0));
    let counter = count.clone();
    let app = Router::new().route(
        "/responses",
        post(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                ([("content-type", "text/event-stream")], sse())
            }
        }),
    );
    let (base, server) = serve(app).await;
    let provider = ResponsesProvider::api_key("synthetic", base).unwrap();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = provider
        .generate(
            request(),
            Arc::new(|_| panic!("pre-cancelled output")),
            cancellation,
        )
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "cancelled");
    assert_eq!(count.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test]
async fn failed_and_error_sse_events_are_typed_and_never_retried() {
    for event_type in ["response.failed", "error"] {
        for (code, expected) in [
            ("server_error", ProviderErrorKind::Transient),
            (
                "rate_limit_exceeded",
                ProviderErrorKind::RateLimit { resets_at: None },
            ),
            ("context_length_exceeded", ProviderErrorKind::Context),
            ("invalid_prompt", ProviderErrorKind::Fatal),
        ] {
            for published in [false, true] {
                let error = json!({"code":code,"message":"SERVER_SECRET_SENTINEL"});
                let event = if event_type == "response.failed" {
                    json!({"type":event_type,"response":{"error":error}})
                } else {
                    json!({"type":event_type,"code":code,"message":"SERVER_SECRET_SENTINEL"})
                };
                let prefix = if published {
                    format!(
                        "data: {}\n\n",
                        json!({"type":"response.output_text.delta","delta":"seen"})
                    )
                } else {
                    String::new()
                };
                let events = format!("{prefix}data: {event}\n\n");
                let count = Arc::new(AtomicUsize::new(0));
                let counter = count.clone();
                let app = Router::new().route(
                    "/responses",
                    post(move || {
                        let events = events.clone();
                        let counter = counter.clone();
                        async move {
                            counter.fetch_add(1, Ordering::SeqCst);
                            ([("content-type", "text/event-stream")], events)
                        }
                    }),
                );
                let (base, server) = serve(app).await;
                let provider = ResponsesProvider::api_key("synthetic", base).unwrap();
                let text = Arc::new(Mutex::new(String::new()));
                let sink = text.clone();
                let error = provider
                    .generate(
                        request(),
                        Arc::new(move |delta| sink.lock().unwrap().push_str(delta)),
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap_err();
                assert_eq!(
                    ProviderError::of(&error).unwrap().kind,
                    expected,
                    "{event_type} {code}"
                );
                assert!(!format!("{error:?}").contains("SERVER_SECRET_SENTINEL"));
                let typed = ProviderError::of(&error).unwrap();
                assert_eq!(typed.published, published);
                let retryable = !published
                    && matches!(
                        expected,
                        ProviderErrorKind::Transient | ProviderErrorKind::RateLimit { .. }
                    );
                assert_eq!(typed.retryable(), retryable);
                assert_eq!(provider_error_retryable(&error), retryable);
                assert_eq!(*text.lock().unwrap(), if published { "seen" } else { "" });
                assert_eq!(count.load(Ordering::SeqCst), 1);
                server.abort();
            }
        }
    }
}

#[tokio::test]
async fn continuation_failure_after_delivery_is_marked_and_never_retried() {
    for status in [
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::OK,
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let app = Router::new().route("/responses", post(move || {
            let counter = counter.clone();
            async move {
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    ([("content-type", "text/event-stream")], format!("data: {}\n\n", json!({"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[{"type":"message","content":[{"type":"output_text","text":"Part "}]}]}}))).into_response()
                } else if status == StatusCode::OK {
                    ([("content-type", "text/event-stream")], format!("data: {}\n\n", json!({"type":"response.completed","response":{"output":[{"type":"function_call","call_id":"bad","name":"workspace_read","arguments":"bad JSON"}],"usage":{"input_tokens":12,"output_tokens":4}}}))).into_response()
                } else {
                    (status, [("retry-after", "3")], Json(json!({"error":{
                        "message":"continuation temporarily unavailable",
                        "api_key":"SERVER_SECRET_SENTINEL",
                        "key_echo":"synthetic"
                    }}))).into_response()
                }
            }
        }));
        let (base, server) = serve(app).await;
        let provider = ResponsesProvider::api_key("synthetic", base).unwrap();
        let text = Arc::new(Mutex::new(String::new()));
        let sink = text.clone();
        let error = provider
            .generate(
                request(),
                Arc::new(move |delta| sink.lock().unwrap().push_str(delta)),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        let typed = ProviderError::of(&error).unwrap();
        assert!(typed.published);
        assert!(!typed.retryable());
        assert!(!provider_error_retryable(&error));
        assert_eq!(*text.lock().unwrap(), "Part ");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(!format!("{error:?}").contains("SERVER_SECRET_SENTINEL"));
        if status != StatusCode::OK {
            let diagnostic = error.to_string();
            assert!(diagnostic.contains("continuation temporarily unavailable"));
            assert!(diagnostic.contains("body="));
            assert!(!diagnostic.contains("synthetic"));
        }
        match status {
            StatusCode::INTERNAL_SERVER_ERROR => {
                assert_eq!(typed.kind, ProviderErrorKind::Transient)
            }
            StatusCode::TOO_MANY_REQUESTS => {
                assert!(matches!(
                    typed.kind,
                    ProviderErrorKind::RateLimit { resets_at: Some(_) }
                ));
                assert!(provider_retry_after(&error).is_some());
            }
            _ => {
                assert_eq!(typed.kind, ProviderErrorKind::Fatal);
                let usage = decision_error_usage(&error)
                    .expect("publication context retains metered usage");
                assert_eq!((usage.input_tokens, usage.output_tokens), (12, 4));
            }
        }
        server.abort();
    }
}

#[tokio::test]
async fn codex_auth_errors_name_the_file_and_never_modify_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("auth.json");
    let missing = ResponsesProvider::codex_auth(&path, "http://127.0.0.1:9")
        .err()
        .unwrap();
    assert_eq!(
        ProviderError::of(&missing).unwrap().kind,
        ProviderErrorKind::Auth
    );
    assert!(missing.to_string().contains(path.to_str().unwrap()));
    assert!(!path.exists());
    for bytes in [
        b"{SERVER_SECRET_SENTINEL".as_slice(),
        br#"{}"#,
        br#"{"tokens":{"access_token":"SERVER_SECRET_SENTINEL"}}"#,
        br#"{"tokens":{"account_id":"synthetic"}}"#,
        br#"{"tokens":{"access_token":"","account_id":"synthetic"}}"#,
        br#"{"tokens":{"access_token":"SERVER_SECRET_SENTINEL","account_id":""}}"#,
        br#"{"tokens":{"access_token":42,"account_id":"synthetic"}}"#,
    ] {
        std::fs::write(&path, bytes).unwrap();
        let error = ResponsesProvider::codex_auth(&path, "http://127.0.0.1:9")
            .err()
            .unwrap();
        assert_eq!(
            ProviderError::of(&error).unwrap().kind,
            ProviderErrorKind::Auth
        );
        assert!(error.to_string().contains(path.to_str().unwrap()));
        assert!(!format!("{error:?}").contains("SERVER_SECRET_SENTINEL"));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
    std::fs::write(
        &path,
        br#"{"tokens":{"access_token":"synthetic","account_id":"synthetic"}}"#,
    )
    .unwrap();
    let provider = ResponsesProvider::codex_auth(&path, "http://127.0.0.1:9").unwrap();
    // A valid construction must not hide a later malformed or removed credential file.
    std::fs::write(&path, b"{SERVER_SECRET_SENTINEL").unwrap();
    let error = provider
        .generate(request(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(
        ProviderError::of(&error).unwrap().kind,
        ProviderErrorKind::Auth
    );
    assert!(error.to_string().contains(path.to_str().unwrap()));
    assert_eq!(std::fs::read(&path).unwrap(), b"{SERVER_SECRET_SENTINEL");
    std::fs::remove_file(&path).unwrap();
    let error = provider
        .generate(request(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(
        ProviderError::of(&error).unwrap().kind,
        ProviderErrorKind::Auth
    );
    assert!(error.to_string().contains(path.to_str().unwrap()));
    assert!(!path.exists());
}

#[tokio::test]
async fn codex_401_rereads_changed_token_once_and_leaves_auth_bytes_unchanged() {
    for (rotate, second_401) in [(true, false), (false, false), (true, true)] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.json");
        let original = br#"{ "tokens": { "access_token": "old-synthetic", "account_id": "old-account" }, "refresh_token": "untouched" }"#;
        let rotated = br#"{ "tokens": { "access_token": "new-synthetic", "account_id": "new-account" }, "refresh_token": "untouched" }"#;
        let rotated_again = br#"{ "tokens": { "access_token": "third-synthetic", "account_id": "third-account" }, "refresh_token": "untouched" }"#;
        std::fs::write(&path, original).unwrap();
        let observed = Arc::new(Mutex::new(Vec::<(HeaderMap, Value)>::new()));
        let sink = observed.clone();
        let auth_path = path.clone();
        let app = Router::new().route(
            "/responses",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let sink = sink.clone();
                let auth_path = auth_path.clone();
                async move {
                    let index = {
                        let mut requests = sink.lock().unwrap();
                        requests.push((headers, body));
                        requests.len()
                    };
                    if index == 1 {
                        if rotate {
                            std::fs::write(auth_path, rotated).unwrap();
                        }
                        (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error":{
                                "message":"credential expired",
                                "access_token":"SERVER_SECRET_SENTINEL",
                                "key_echo":"old-synthetic"
                            }})),
                        )
                            .into_response()
                    } else if second_401 {
                        // A second rotation must not cause a second auth retry.
                        std::fs::write(auth_path, rotated_again).unwrap();
                        (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error":{
                                "message":"credential expired",
                                "access_token":"SERVER_SECRET_SENTINEL",
                                "key_echo":"new-synthetic"
                            }})),
                        )
                            .into_response()
                    } else {
                        ([("content-type", "text/event-stream")], sse()).into_response()
                    }
                }
            }),
        );
        let (base, server) = serve(app).await;
        // Token-change retry is allowed even when ordinary transport retries are disabled.
        let provider = ResponsesProvider::codex_auth(&path, base).unwrap();
        let result = provider
            .generate(request(), Arc::new(|_| {}), CancellationToken::new())
            .await;
        if rotate && !second_401 {
            assert_eq!(result.unwrap().text, "Reading.");
        } else {
            let error = result.unwrap_err();
            assert_eq!(
                ProviderError::of(&error).unwrap().kind,
                ProviderErrorKind::Auth
            );
            assert!(error.to_string().contains("Codex CLI owns token refresh"));
            assert!(error.to_string().contains("codex login"));
            assert!(error.to_string().contains(path.to_str().unwrap()));
            assert!(!format!("{error:?}").contains("SERVER_SECRET_SENTINEL"));
            assert!(error.to_string().contains("credential expired"));
            assert!(error.to_string().contains("body="));
            assert!(!error.to_string().contains("old-synthetic"));
            assert!(!error.to_string().contains("new-synthetic"));
        }
        assert_eq!(
            std::fs::read(&path).unwrap(),
            if second_401 {
                rotated_again.as_slice()
            } else if rotate {
                rotated.as_slice()
            } else {
                original.as_slice()
            }
        );
        let observed = observed.lock().unwrap();
        assert_eq!(observed.len(), if rotate { 2 } else { 1 });
        assert_eq!(observed[0].0["authorization"], "Bearer old-synthetic");
        if rotate {
            assert_eq!(observed[1].0["authorization"], "Bearer new-synthetic");
            assert_eq!(observed[1].0["chatgpt-account-id"], "new-account");
            assert_eq!(
                observed[0].0["idempotency-key"],
                observed[1].0["idempotency-key"]
            );
            assert_eq!(
                serde_json::to_vec(&observed[0].1).unwrap(),
                serde_json::to_vec(&observed[1].1).unwrap()
            );
        }
        server.abort();
    }
}

#[tokio::test]
async fn responses_to_a_plain_openai_route_omits_the_session_header() {
    let observed = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    let seen = observed.clone();
    let app = Router::new().route(
        "/v1/responses",
        post(move |headers: HeaderMap| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push(
                    headers
                        .get("x-opencode-session")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned),
                );
                let event = json!({"type":"response.completed","response":{"output":[{"type":"message","content":[{"type":"output_text","text":"OK"}]}],"usage":{"input_tokens":4,"output_tokens":1}}});
                ([("content-type", "text/event-stream")], format!("data: {event}

"))
            }
        }),
    );
    let (base, server) = serve(app).await;
    let provider = ResponsesProvider::api_key("synthetic-key", format!("{base}/v1")).unwrap();
    let mut explicit = request();
    explicit.session_id = "explicit-session".into();
    provider
        .generate(explicit, Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    server.abort();
    let seen = observed.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0].is_none(),
        "plain OpenAI route got the OpenCode header"
    );
}
