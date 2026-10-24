//! The current rejection supplies scheduling evidence; earlier attempts do not.
use axum::{Router, http::StatusCode, response::IntoResponse, routing::post};
use bubblepaw_core::{
    CancellationToken, Message, ModelRequest, Provider, ProviderError, ProviderErrorKind, Role,
    Store,
};
use bubblepaw_runner::provider::{
    MessagesProvider, ProviderRegistry, ResponsesProvider, RetryPolicy,
};
use bubblepaw_runner::quota::{LimitAction, QuotaHost, classify_limit};
use chrono::{DateTime, Utc};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, server)
}

fn codex_auth(path: &std::path::Path) {
    std::fs::write(
        path,
        json!({"tokens":{"access_token":"synthetic","account_id":"synthetic"}}).to_string(),
    )
    .unwrap();
}

fn request() -> ModelRequest {
    ModelRequest {
        model: "synthetic".into(),
        messages: vec![Message {
            id: "quota-user".into(),
            session_id: "quota-session".into(),
            role: Role::User,
            origin: None,
            content: "hello".into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: "2026-10-21T12:00:00Z".into(),
        }],
        ..Default::default()
    }
}

#[tokio::test]
async fn retry_readings_do_not_name_a_later_429() {
    for current_headers in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let app = Router::new().route(
            "/responses",
            post(move || {
                let ordinal = counter.fetch_add(1, Ordering::SeqCst);
                async move {
                    if ordinal == 0 {
                        // An exhausted reading alone must not fail a transient 503.
                        (
                            StatusCode::SERVICE_UNAVAILABLE,
                            [
                                ("x-codex-primary-used-percent", "100"),
                                ("x-codex-primary-reset-after-seconds", "36000"),
                            ],
                            axum::Json(json!({"error":{"code":"server_error"}})),
                        )
                            .into_response()
                    } else if current_headers {
                        (
                            StatusCode::TOO_MANY_REQUESTS,
                            [
                                ("x-codex-secondary-used-percent", "100"),
                                ("x-codex-secondary-reset-after-seconds", "600"),
                            ],
                            axum::Json(json!({"error":{"code":"rate_limit_exceeded"}})),
                        )
                            .into_response()
                    } else {
                        (
                            StatusCode::TOO_MANY_REQUESTS,
                            axum::Json(json!({"error":{"code":"rate_limit_exceeded"}})),
                        )
                            .into_response()
                    }
                }
            }),
        );
        let (base, server) = serve(app).await;
        let directory = tempfile::tempdir().unwrap();
        let auth = directory.path().join("auth.json");
        codex_auth(&auth);
        let provider = ResponsesProvider::codex_auth(auth, base).unwrap();
        // The registry's policy retries the 503 once; longer waits are not retried.
        let mut registry = ProviderRegistry::new("codex", Arc::new(provider));
        registry.set_retry_policy(RetryPolicy {
            max_attempts: 2,
            initial_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        });
        let error = registry
            .generate(request(), Arc::new(|_| {}), CancellationToken::new())
            .await
            .unwrap_err();
        let typed = ProviderError::of(&error).unwrap();
        assert!(matches!(typed.kind, ProviderErrorKind::RateLimit { .. }));
        assert_eq!(typed.quota.len(), 1 + usize::from(current_headers));
        if current_headers {
            assert_eq!(typed.quota[0].window, "secondary");
            let reset = DateTime::parse_from_rfc3339(typed.quota[0].resets_at.as_deref().unwrap())
                .unwrap()
                .with_timezone(&Utc);
            assert_eq!(
                classify_limit(
                    &typed.quota[0].account,
                    "synthetic",
                    &error,
                    &[],
                    Utc::now(),
                    Duration::from_secs(3600)
                ),
                LimitAction::Park {
                    resume_at: reset,
                    window: "secondary".into()
                }
            );
            assert_eq!(typed.quota[1].window, "primary");
        } else {
            assert_eq!(typed.quota[0].window, "primary");
            assert_eq!(
                classify_limit(
                    &typed.quota[0].account,
                    "synthetic",
                    &error,
                    &[],
                    Utc::now(),
                    Duration::from_secs(3600)
                ),
                LimitAction::Short(Duration::ZERO),
                "the earlier 503's quota must not name the current rejection"
            );
            // The host observes retained readings before using its catalog as
            // fallback; account normalization must preserve the empty boundary.
            let store = Store::in_memory().unwrap();
            let host = QuotaHost::new(store.clone(), Duration::ZERO).unwrap();
            let mut pending = Some(error);
            let error = host
                .execute(
                    "alias",
                    "shared",
                    "synthetic",
                    CancellationToken::new(),
                    || {
                        std::future::ready(Err::<bubblepaw_core::ModelResponse, _>(
                            pending.take().expect("no second dispatch"),
                        ))
                    },
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains("window primary"));
            assert!(error.to_string().contains("maximum wait exceeded"));
            assert_eq!(host.latest("shared").len(), 1);
            assert_eq!(store.quota_history().unwrap().len(), 1);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        server.abort();
    }
}

#[tokio::test]
async fn successful_continuation_readings_do_not_name_a_later_429() {
    for adapter in ["responses", "messages"] {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let path = if adapter == "responses" {
            "/responses"
        } else {
            "/messages"
        };
        let app = Router::new().route(path, post(move || {
            let ordinal = counter.fetch_add(1, Ordering::SeqCst);
            async move {
                match (adapter, ordinal) {
                    ("responses", 0) => {
                        let body = format!("data: {}\n\ndata: {}\n\n",
                            json!({"type":"response.output_text.delta","delta":"visible"}),
                            json!({"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[],"usage":{"input_tokens":5,"output_tokens":1}}}));
                        (StatusCode::OK, [
                            ("content-type", "text/event-stream"),
                            ("x-codex-primary-used-percent", "100"),
                            ("x-codex-primary-reset-after-seconds", "60"),
                        ], body).into_response()
                    }
                    ("responses", _) => (StatusCode::TOO_MANY_REQUESTS, [
                        ("x-codex-secondary-used-percent", "100"),
                        ("x-codex-secondary-reset-after-seconds", "600"),
                    ], axum::Json(json!({"error":{"code":"rate_limit_exceeded"}}))).into_response(),
                    ("messages", 0) => (StatusCode::OK, [
                        ("anthropic-ratelimit-requests-limit", "100"),
                        ("anthropic-ratelimit-requests-remaining", "0"),
                        ("anthropic-ratelimit-requests-reset", "2028-01-01T00:00:00Z"),
                    ], axum::Json(json!({"content":[{"type":"text","text":"visible"}],"stop_reason":"max_tokens","usage":{"input_tokens":5,"output_tokens":1}}))).into_response(),
                    _ => (StatusCode::TOO_MANY_REQUESTS, [
                        ("anthropic-ratelimit-tokens-limit", "100"),
                        ("anthropic-ratelimit-tokens-remaining", "0"),
                        ("anthropic-ratelimit-tokens-reset", "2029-01-01T00:00:00Z"),
                    ], axum::Json(json!({"error":{"type":"rate_limit_error"}}))).into_response(),
                }
            }
        }));
        let (base, server) = serve(app).await;
        let directory = tempfile::tempdir().unwrap();
        let auth = directory.path().join("auth.json");
        codex_auth(&auth);
        let provider: Box<dyn Provider> = if adapter == "responses" {
            Box::new(ResponsesProvider::codex_auth(auth, base).unwrap())
        } else {
            Box::new(MessagesProvider::api_key("synthetic", base).unwrap())
        };
        let error = provider
            .generate(request(), Arc::new(|_| {}), CancellationToken::new())
            .await
            .unwrap_err();
        let typed = ProviderError::of(&error).unwrap();
        assert!(matches!(typed.kind, ProviderErrorKind::RateLimit { .. }));
        assert!(
            typed.published,
            "published output must keep its replay guard"
        );
        assert_eq!(typed.quota.len(), 2);
        assert_eq!(
            typed.quota[0].window,
            if adapter == "responses" {
                "secondary"
            } else {
                "tokens"
            }
        );
        assert_eq!(
            typed.quota[1].window,
            if adapter == "responses" {
                "primary"
            } else {
                "requests"
            }
        );
        let account = typed.quota[0].account.clone();
        let window = typed.quota[0].window.clone();
        let reset = DateTime::parse_from_rfc3339(typed.quota[0].resets_at.as_deref().unwrap())
            .unwrap()
            .with_timezone(&Utc);
        // Exercise the evidence boundary through continuation/publication
        // contexts; the actual published failure above remains non-retryable.
        let unpublished = typed.clone().with_published(false);
        let policy_error = error.context(unpublished);
        assert_eq!(
            classify_limit(
                &account,
                "synthetic",
                &policy_error,
                &[],
                Utc::now(),
                Duration::from_secs(10 * 365 * 86400)
            ),
            LimitAction::Park {
                resume_at: reset,
                window
            },
            "an earlier continuation reset must not replace the current rejection"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        server.abort();
    }
}
