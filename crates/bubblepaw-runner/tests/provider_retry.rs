use axum::{Router, http::StatusCode, routing::post};
use bubblepaw_core::{
    CancellationToken, Message, ModelRequest, Provider, ProviderError, ProviderErrorKind, Role,
    ToolRegistry,
};
use bubblepaw_runner::provider::{
    ChatCompletionsProvider, MessagesProvider, ProviderRegistry, ResponsesProvider, RetryPolicy,
    parse_retry_after, provider_retry_after,
};
use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

#[path = "support/worker_jobs.rs"]
mod worker_jobs;

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

#[test]
fn retry_after_supports_seconds_and_http_dates() {
    assert_eq!(parse_retry_after(" 120 "), Some(Duration::from_secs(120)));
    assert_eq!(parse_retry_after("0"), Some(Duration::ZERO));
    for invalid in ["", "-1", "1.5", "+1", "tomorrow", "18446744073709551616"] {
        assert_eq!(parse_retry_after(invalid), None);
    }
    let future = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(120));
    let delay = parse_retry_after(&future).unwrap();
    assert!(delay > Duration::from_secs(115) && delay <= Duration::from_secs(120));
    assert_eq!(
        parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT"),
        Some(Duration::ZERO)
    );
    assert_eq!(
        parse_retry_after("Sunday, 06-Nov-94 08:49:37 GMT"),
        Some(Duration::ZERO)
    );
    assert_eq!(
        parse_retry_after("Sun Nov  6 08:49:37 1994"),
        Some(Duration::ZERO)
    );
    assert_eq!(
        provider_retry_after(&anyhow::anyhow!("no retry metadata")),
        None
    );
}

#[tokio::test]
async fn retryable_server_errors_keep_header_and_typed_source_for_all_adapters() {
    for header in [
        "120".to_owned(),
        httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(120)),
    ] {
        for adapter in ["messages", "responses", "chat"] {
            let path = match adapter {
                "messages" => "/messages",
                "responses" => "/responses",
                _ => "/chat/completions",
            };
            let header = header.clone();
            let app = Router::new().route(
                path,
                post(move || {
                    let header = header.clone();
                    async move { (StatusCode::SERVICE_UNAVAILABLE, [("retry-after", header)]) }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let provider: Box<dyn Provider> = match adapter {
                "messages" => Box::new(MessagesProvider::api_key("synthetic", base).unwrap()),
                "responses" => Box::new(ResponsesProvider::api_key("synthetic", base).unwrap()),
                _ => Box::new(ChatCompletionsProvider::api_key("synthetic", base).unwrap()),
            };
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let error = provider
                .generate(request(), Arc::new(|_| {}), CancellationToken::new())
                .await
                .unwrap_err()
                .context("host context");
            assert_eq!(
                ProviderError::of(&error).unwrap().kind,
                ProviderErrorKind::Transient
            );
            let delay = provider_retry_after(&error).unwrap();
            assert!(
                delay > Duration::from_secs(110) && delay <= Duration::from_secs(120),
                "{adapter}: {delay:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert!(provider_retry_after(&error).unwrap() < delay);
            server.abort();
        }
    }
}

/// A server wait above the policy's range returns at once, carrying its
/// Retry-After; a long wait belongs to the quota host or the caller.
#[tokio::test]
async fn responses_server_delay_above_max_delay_is_not_retried_inline() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = calls.clone();
    let app = Router::new().route(
        "/responses",
        post(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                (StatusCode::SERVICE_UNAVAILABLE, [("retry-after", "120")])
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let provider = ResponsesProvider::api_key("synthetic", base).unwrap();
    let mut registry = ProviderRegistry::new("responses", Arc::new(provider));
    registry.set_retry_policy(RetryPolicy {
        max_attempts: 3,
        initial_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(1),
    });
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        registry.generate(request(), Arc::new(|_| {}), CancellationToken::new()),
    )
    .await
    .expect("a server wait above max_delay must not be slept inline")
    .unwrap_err();
    assert_eq!(
        ProviderError::of(&error).unwrap().kind,
        ProviderErrorKind::Transient
    );
    let delay = provider_retry_after(&error).unwrap();
    assert!(
        delay > Duration::from_secs(110) && delay <= Duration::from_secs(120),
        "{delay:?}"
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn chat_precancelled_request_never_reaches_transport() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider = ChatCompletionsProvider::api_key(
        "synthetic",
        format!("http://{}", listener.local_addr().unwrap()),
    )
    .unwrap();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        provider.generate(request(), Arc::new(|_| {}), cancellation),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(error.to_string(), "cancelled");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err(),
        "pre-cancelled request opened a connection"
    );
}

/// A worker's server wait above its max backoff fails at once instead of
/// sleeping or retrying: long waits are not retried inline.
#[tokio::test]
async fn worker_server_wait_above_max_backoff_fails_without_another_attempt() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = calls.clone();
    let app = Router::new().route(
        "/chat/completions",
        post(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                (StatusCode::SERVICE_UNAVAILABLE, [("retry-after", "120")])
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider = ChatCompletionsProvider::api_key(
        "synthetic",
        format!("http://{}", listener.local_addr().unwrap()),
    )
    .unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let registry =
        ProviderRegistry::new("synthetic", Arc::new(provider)).with_retry_policy(RetryPolicy {
            max_attempts: 3,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
        });
    let fixture =
        worker_jobs::Fixture::new(Arc::new(registry), ToolRegistry::new(), &request().model);
    let run = tokio::time::timeout(Duration::from_secs(5), fixture.run("Hello"))
        .await
        .expect("the run must not wait out a server delay above its maximum");
    assert_eq!(run.status, "failed");
    let message = run.error.unwrap_or_default();
    assert!(
        message.contains("503"),
        "the fetched error must be reported: {message}"
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    server.abort();
}

#[derive(Clone)]
struct RouteFixture {
    calls: Arc<std::sync::Mutex<Vec<ModelRequest>>>,
    failure: Option<&'static str>,
    visible: bool,
    cancel: bool,
}
#[async_trait::async_trait]
impl Provider for RouteFixture {
    async fn generate(
        &self,
        request: ModelRequest,
        text: bubblepaw_core::TextCallback,
        cancellation: CancellationToken,
    ) -> anyhow::Result<bubblepaw_core::ModelResponse> {
        self.calls.lock().unwrap().push(request);
        if self.visible {
            text("already visible");
        }
        if self.cancel {
            cancellation.cancel();
        }
        if let Some(message) = self.failure {
            let mut error = ProviderError::new(ProviderErrorKind::Auth, message);
            error.usage = Some(bubblepaw_core::Usage {
                input_tokens: 3,
                output_tokens: 1,
                ..Default::default()
            });
            return Err(error.into());
        }
        Ok(bubblepaw_core::ModelResponse {
            text: "paid or free success".into(),
            tool_calls: vec![],
            usage: Some(bubblepaw_core::Usage {
                input_tokens: 11,
                output_tokens: 2,
                ..Default::default()
            }),
            last_request_usage: None,
            quota: Vec::new(),
            provider_state: None,
        })
    }
}
fn fallback_registry(
    failure: Option<&'static str>,
    visible: bool,
    cancel: bool,
) -> (
    bubblepaw_runner::provider::ProviderRegistry,
    RouteFixture,
    RouteFixture,
) {
    use bubblepaw_runner::provider::ProviderRegistry;
    let free = RouteFixture {
        calls: Default::default(),
        failure,
        visible,
        cancel,
    };
    let paid = RouteFixture {
        calls: Default::default(),
        failure: None,
        visible: false,
        cancel: false,
    };
    let mut registry = ProviderRegistry::new("free", Arc::new(free.clone())).with_strict_routes();
    registry.insert("paid", Arc::new(paid.clone())).unwrap();
    registry.configure_route("free", "https://free.invalid/v1".into(), Default::default());
    registry.configure_route("paid", "https://paid.invalid/v1".into(), Default::default());
    registry
        .set_model_fallbacks("free/flash-free".into(), vec!["paid/flash".into()])
        .unwrap();
    (registry, free, paid)
}
#[tokio::test]
async fn configured_fallback_is_sticky_metered_and_records_selected_route_for_stateless_aux() {
    use bubblepaw_runner::provider::ProviderRegistry;
    let (registry, free, paid) = fallback_registry(Some("FreeTierError"), false, false);
    let mut req = request();
    req.model = "free/flash-free".into();
    // Auxiliary calls use a real source session but no stateful provider session.
    assert!(req.session_id.is_empty());
    let response = registry
        .generate(req.clone(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        ProviderRegistry::selected_model(&response),
        Some("paid/flash")
    );
    let usage = response.usage.unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens), (14, 3));
    let last = response.last_request_usage.unwrap();
    assert_eq!((last.input_tokens, last.output_tokens), (Some(11), Some(2)));
    assert_eq!(
        response.provider_state.as_ref().unwrap()["_bubblepaw_route_origin"]["provider"],
        "paid"
    );
    assert_eq!(
        response.provider_state.as_ref().unwrap()["_bubblepaw_fallback_from"],
        serde_json::json!(["free/flash-free"])
    );
    registry
        .generate(req, Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(free.calls.lock().unwrap().len(), 1);
    let calls = paid.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|r| r.session_id.is_empty()
        && r.model == "flash"
        && r.messages[0].session_id == "fixture"));
}
#[tokio::test]
async fn successful_free_route_never_uses_paid_and_stop_conditions_never_fall_through() {
    for (failure, visible, cancel) in [
        (None, false, false),
        (Some("partial transport failure"), true, false),
        (Some("cancelled"), false, true),
    ] {
        let (registry, free, paid) = fallback_registry(failure, visible, cancel);
        let mut req = request();
        req.model = "free/flash-free".into();
        let result = registry
            .generate(req, Arc::new(|_| {}), CancellationToken::new())
            .await;
        assert_eq!(result.is_ok(), failure.is_none());
        if let Err(error) = result {
            assert_eq!(ProviderError::of(&error).unwrap().published, visible);
        }
        assert_eq!(free.calls.lock().unwrap().len(), 1);
        assert!(paid.calls.lock().unwrap().is_empty());
    }
    let (registry, free, paid) = fallback_registry(Some("unused"), false, false);
    let token = CancellationToken::new();
    token.cancel();
    let mut req = request();
    req.model = "free/flash-free".into();
    assert!(
        registry
            .generate(req, Arc::new(|_| {}), token)
            .await
            .is_err()
    );
    assert!(free.calls.lock().unwrap().is_empty());
    assert!(paid.calls.lock().unwrap().is_empty());
}
#[tokio::test]
async fn chat_free_403_falls_back_once_and_preserves_http_failure_usage() {
    use axum::{Json, extract::State, http::HeaderMap};
    use bubblepaw_runner::provider::ProviderRegistry;
    use serde_json::{Value, json};
    type Seen = Arc<std::sync::Mutex<Vec<(String, String)>>>;
    let seen: Seen = Default::default();
    async fn upstream(
        State(seen): State<Seen>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        let model = body["model"].as_str().unwrap().to_owned();
        seen.lock().unwrap().push((
            model.clone(),
            headers["x-opencode-session"].to_str().unwrap().to_owned(),
        ));
        if model.ends_with("-free") {
            (
                StatusCode::FORBIDDEN,
                Json(
                    json!({"error":{"type":"FreeTierError"},"usage":{"prompt_tokens":3,"completion_tokens":1}}),
                ),
            )
        } else {
            (
                StatusCode::OK,
                Json(
                    json!({"choices":[{"message":{"content":"done"},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"completion_tokens":2}}),
                ),
            )
        }
    }
    let app = Router::new()
        .route("/chat/completions", post(upstream))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let provider = Arc::new(
        ChatCompletionsProvider::api_key("synthetic", base)
            .unwrap()
            .with_opencode_route(),
    );
    let mut registry =
        ProviderRegistry::new("opencode-free", provider.clone()).with_strict_routes();
    registry.insert("opencode", provider).unwrap();
    registry
        .set_model_fallbacks(
            "opencode-free/deepseek-v4.1-flash-free".into(),
            vec!["opencode/deepseek-v4.1-flash".into()],
        )
        .unwrap();
    let mut req = request();
    req.model = "opencode-free/deepseek-v4.1-flash-free".into();
    let result = registry
        .generate(req, Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.text, "done");
    assert_eq!(result.usage.unwrap().input_tokens, 14);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            ("deepseek-v4.1-flash-free".into(), "fixture".into()),
            ("deepseek-v4.1-flash".into(), "fixture".into())
        ]
    );
    server.abort();
}
