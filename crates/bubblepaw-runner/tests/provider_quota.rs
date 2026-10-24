//! Local HTTP acceptance: no provider credentials or paid model calls.
use axum::{Router, http::StatusCode, response::IntoResponse, routing::post};
use bubblepaw_core::{
    CancellationToken, Message, ModelRequest, Provider, ProviderError, ProviderErrorKind,
    QuotaReading, QuotaSource, Role, Store,
};
use bubblepaw_runner::{
    provider::{
        MessagesProvider, ProviderRegistry, ResponsesProvider, RetryPolicy, is_terminal_quota_error,
    },
    quota::QuotaHost,
};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

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
            created_at: "2026-10-20T12:00:00Z".into(),
        }],
        ..Default::default()
    }
}
async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, task)
}
fn codex_auth(path: &std::path::Path) {
    std::fs::write(
        path,
        json!({"tokens":{"access_token":"synthetic-access","account_id":"synthetic-account"}})
            .to_string(),
    )
    .unwrap();
}
#[tokio::test]
async fn codex_success_and_exhausted_failure_keep_response_headers_without_internal_retry() {
    for exhausted in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let app = Router::new().route("/responses", post(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async move {
                let (status, body) = if exhausted { (StatusCode::TOO_MANY_REQUESTS, json!({"error":{"code":"rate_limit_exceeded"}}).to_string()) } else {
                    (StatusCode::OK, format!("data: {}\n\n", json!({"type":"response.completed","response":{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}],"usage":{"input_tokens":4,"output_tokens":1}}})))
                };
                (status, [("content-type", "text/event-stream"), ("x-codex-primary-used-percent", if exhausted { "100" } else { "72" }), ("x-codex-primary-window-minutes", "300"), ("x-codex-primary-reset-after-seconds", "3600")], body).into_response()
            }
        }));
        let (base, server) = serve(app).await;
        let directory = tempfile::tempdir().unwrap();
        let auth = directory.path().join("auth.json");
        codex_auth(&auth);
        let provider = ResponsesProvider::codex_auth(&auth, base).unwrap();
        let result = provider
            .generate(request(), Arc::new(|_| {}), CancellationToken::new())
            .await;
        let quota = if exhausted {
            let error = result.unwrap_err();
            let typed = ProviderError::of(&error).unwrap();
            assert!(matches!(typed.kind, ProviderErrorKind::RateLimit { .. }));
            typed.quota.clone()
        } else {
            result.unwrap().quota
        };
        assert_eq!(quota.len(), 1);
        assert_eq!(quota[0].window, "primary");
        assert_eq!(quota[0].used_percent, if exhausted { 100.0 } else { 72.0 });
        assert!(quota[0].resets_at.is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        server.abort();
    }
}
#[tokio::test]
async fn codex_changing_balances_update_latest_without_history_rows() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let app = Router::new().route("/responses", post(move || {
        let index = counter.fetch_add(1, Ordering::SeqCst);
        async move {
            let body = format!("data: {}\n\n", json!({"type":"response.completed","response":{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}],"usage":{"input_tokens":4,"output_tokens":1}}}));
            let headers = [
                ("content-type", "text/event-stream".to_owned()),
                ("x-codex-credits-has-credits", "true".to_owned()),
                ("x-codex-credits-unlimited", "false".to_owned()),
                ("x-codex-credits-balance", format!("{:.2}", 100.0 - index as f64)),
            ];
            (StatusCode::OK, headers, body).into_response()
        }
    }));
    let (base, server) = serve(app).await;
    let directory = tempfile::tempdir().unwrap();
    let auth = directory.path().join("auth.json");
    codex_auth(&auth);
    let provider = ResponsesProvider::codex_auth(&auth, base).unwrap();
    // A data-directory store counts catalog transactions, which the assertion needs.
    let store = Store::open_data_dir(directory.path().join("data")).unwrap();
    let host = QuotaHost::new(store.clone(), Duration::ZERO).unwrap();
    let before = store.storage_stats().unwrap();
    for _ in 0..10 {
        let response = host
            .execute(
                "codex",
                "personal",
                "synthetic",
                CancellationToken::new(),
                || provider.generate(request(), Arc::new(|_| {}), CancellationToken::new()),
            )
            .await
            .unwrap();
        assert_eq!(response.quota.len(), 1);
        assert_eq!(response.quota[0].window, "credits");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 10);
    let latest = host.latest("personal");
    assert_eq!(latest.len(), 1);
    assert_eq!(latest[0].window, "credits");
    assert_eq!(latest[0].used_percent, 0.0);
    assert_eq!(latest[0].balance.as_deref(), Some("91.00"));
    let history = store.quota_history().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].window, "credits");
    // The one change row keeps the balance it was written with; later
    // balance-only changes stay in host memory.
    assert_eq!(history[0].balance.as_deref(), Some("100.00"));
    assert_eq!(
        store.storage_stats().unwrap().catalog_transaction_count - before.catalog_transaction_count,
        1
    );
    server.abort();
}
#[tokio::test]
async fn codex_zero_relative_reset_stays_unknown_on_exhausted_response() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let app = Router::new().route(
        "/responses",
        post(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async move {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [
                        ("content-type", "text/event-stream"),
                        ("x-codex-primary-used-percent", "100"),
                        ("x-codex-primary-window-minutes", "300"),
                        ("x-codex-primary-reset-after-seconds", "0"),
                    ],
                    json!({"error":{"code":"rate_limit_exceeded"}}).to_string(),
                )
                    .into_response()
            }
        }),
    );
    let (base, server) = serve(app).await;
    let directory = tempfile::tempdir().unwrap();
    let auth = directory.path().join("auth.json");
    codex_auth(&auth);
    let provider = ResponsesProvider::codex_auth(&auth, base).unwrap();
    let error = provider
        .generate(request(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap_err();
    let typed = ProviderError::of(&error).unwrap();
    assert_eq!(typed.quota.len(), 1);
    assert_eq!(typed.quota[0].used_percent, 100.0);
    assert!(typed.quota[0].resets_at.is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.abort();
}
#[tokio::test]
async fn anthropic_success_and_429_keep_headers_but_opencode_has_no_meter() {
    for (exhausted, opencode) in [(false, false), (true, false), (false, true)] {
        let app = Router::new().route("/messages", post(move || async move {
            let status = if exhausted { StatusCode::TOO_MANY_REQUESTS } else { StatusCode::OK };
            let body = if exhausted { json!({"error":{"type":"rate_limit_error"}}) } else { json!({"content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn","usage":{"input_tokens":4,"output_tokens":1}}) };
            (status, [("anthropic-ratelimit-requests-limit", "200"), ("anthropic-ratelimit-requests-remaining", if exhausted { "0" } else { "50" }), ("anthropic-ratelimit-requests-reset", "2026-10-21T00:00:00Z")], axum::Json(body))
        }));
        let (base, server) = serve(app).await;
        let mut provider = MessagesProvider::api_key("synthetic-key", base).unwrap();
        if opencode {
            provider = provider.with_opencode_route();
        }
        let result = provider
            .generate(request(), Arc::new(|_| {}), CancellationToken::new())
            .await;
        let quota = if exhausted {
            ProviderError::of(&result.unwrap_err())
                .unwrap()
                .quota
                .clone()
        } else {
            result.unwrap().quota
        };
        if opencode {
            assert!(quota.is_empty());
        } else {
            assert_eq!(quota.len(), 1);
            assert_eq!(quota[0].used_percent, if exhausted { 100.0 } else { 75.0 });
            assert_eq!(quota[0].source, QuotaSource::ResponseHeaders);
        }
        server.abort();
    }
}
#[tokio::test]
async fn billing_429_is_fatal_and_never_retried() {
    for marker in [
        "enforced_spend_limit_reached",
        "billing_error",
        "insufficient_quota",
        "credits_required",
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let app = Router::new().route(
            "/responses",
            post(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                async move {
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        axum::Json(
                            json!({"error":{"code":marker,"message":"synthetic rejection"}}),
                        ),
                    )
                }
            }),
        );
        let (base, server) = serve(app).await;
        let provider = ResponsesProvider::api_key("synthetic-key", base).unwrap();
        let error = provider
            .generate(request(), Arc::new(|_| {}), CancellationToken::new())
            .await
            .unwrap_err();
        let typed = ProviderError::of(&error).unwrap();
        assert!(is_terminal_quota_error(typed));
        assert!(!typed.retryable());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        server.abort();
    }
}

#[tokio::test]
async fn provider_explicit_nonretryable_429_is_fatal_and_never_retried() {
    for rejection in [
        json!({"error":{"type":"rate_limit_error","retryable":false}}),
        json!({"error":{"type":"rate_limit_error"},"retryable":false}),
        json!({"error":{"type":"rate_limit_error","non_retryable":true}}),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let app = Router::new().route(
            "/responses",
            post(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                let rejection = rejection.clone();
                async move { (StatusCode::TOO_MANY_REQUESTS, axum::Json(rejection)) }
            }),
        );
        let (base, server) = serve(app).await;
        let provider = ResponsesProvider::api_key("synthetic-key", base).unwrap();
        let error = provider
            .generate(request(), Arc::new(|_| {}), CancellationToken::new())
            .await
            .unwrap_err();
        let typed = ProviderError::of(&error).unwrap();
        assert!(is_terminal_quota_error(typed));
        assert!(!typed.retryable());
        assert!(
            typed
                .message
                .starts_with("provider marked request non-retryable")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        server.abort();
    }
}

#[tokio::test]
async fn cancellation_during_responses_backoff_keeps_completed_attempt_readings() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let app = Router::new().route(
        "/responses",
        post(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async move {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [
                        ("x-codex-primary-used-percent", "42"),
                        ("retry-after", "60"),
                    ],
                    axum::Json(json!({"error":{"code":"server_error"}})),
                )
            }
        }),
    );
    let (base, server) = serve(app).await;
    let directory = tempfile::tempdir().unwrap();
    let auth = directory.path().join("auth.json");
    codex_auth(&auth);
    let provider = ResponsesProvider::codex_auth(&auth, base).unwrap();
    let mut registry = ProviderRegistry::new("codex", Arc::new(provider));
    // The registry retries the 503; the server's 60 s wait is inside its range.
    registry.set_retry_policy(RetryPolicy {
        max_attempts: 3,
        initial_delay: Duration::from_millis(1),
        max_delay: Duration::from_secs(120),
    });
    let cancellation = CancellationToken::new();
    let work = bubblepaw_core::capture_request_usage(
        "quota-session",
        None,
        bubblepaw_core::RequestKind::Main,
        async {
            let generating = registry.generate(request(), Arc::new(|_| {}), cancellation.clone());
            tokio::pin!(generating);
            tokio::select! {
                result = &mut generating => result,
                _ = async {
                    while bubblepaw_core::request_usage_count() != Some(1) { tokio::task::yield_now().await; }
                    cancellation.cancel();
                } => generating.await,
            }
        },
    );
    let (result, attempts) = tokio::time::timeout(Duration::from_secs(5), work)
        .await
        .unwrap();
    let error = result.unwrap_err();
    let typed = ProviderError::of(&error).unwrap();
    assert_eq!(typed.message, "cancelled");
    assert_eq!(typed.quota.len(), 1);
    assert_eq!(typed.quota[0].used_percent, 42.0);
    assert_eq!(attempts.len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.abort();
}

#[test]
fn changed_meter_carries_freshest_balance_from_a_later_batch_entry() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("data");
    let credits = |at: &str, balance: &str| QuotaReading {
        provider: "codex".into(),
        account: "personal".into(),
        window: "credits".into(),
        used_percent: 0.0,
        resets_at: None,
        observed_at: at.into(),
        source: QuotaSource::ResponseHeaders,
        balance: Some(balance.into()),
    };
    let store = Store::open_data_dir(&data).unwrap();
    let host = QuotaHost::new(store.clone(), Duration::ZERO).unwrap();
    host.observe(
        "codex",
        "personal",
        &[credits("2026-10-21T00:00:00Z", "100.00")],
    )
    .unwrap();
    // The credit observation follows the changed secondary meter in the batch.
    let mut secondary = credits("2026-10-21T00:00:10Z", "90.00");
    secondary.window = "secondary".into();
    secondary.used_percent = 25.0;
    secondary.balance = None;
    host.observe(
        "codex",
        "personal",
        &[secondary, credits("2026-10-21T00:00:10Z", "90.00")],
    )
    .unwrap();
    let rows = store.quota_history().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].window, "secondary");
    assert_eq!(rows[1].balance.as_deref(), Some("90.00"));
    assert_eq!(rows[1].observed_at, "2026-10-21T00:00:10Z");
    let known = ("90.00".to_owned(), "2026-10-21T00:00:10Z".to_owned());
    assert_eq!(host.latest_balance("personal"), Some(known.clone()));

    // An older balance arrival never replaces the newer one.
    host.observe(
        "codex",
        "personal",
        &[credits("2026-10-21T00:00:05Z", "7.00")],
    )
    .unwrap();
    assert_eq!(host.latest_balance("personal"), Some(known.clone()));
    drop(host);
    drop(store);

    // After reopening, the host resumes from the stored balance.
    let store = Store::open_data_dir(&data).unwrap();
    let host = QuotaHost::new(store.clone(), Duration::ZERO).unwrap();
    assert_eq!(host.latest_balance("personal"), Some(known));
    // latest() returns the balance each stored row carried, not an overlay.
    let secondary = host
        .latest("personal")
        .into_iter()
        .find(|row| row.window == "secondary")
        .unwrap();
    assert_eq!(secondary.balance.as_deref(), Some("90.00"));
    assert_eq!(secondary.observed_at, "2026-10-21T00:00:10Z");
}

fn credit_reading(at: &str, balance: &str, used_percent: f64) -> QuotaReading {
    QuotaReading {
        provider: "codex".into(),
        account: "personal".into(),
        window: "credits".into(),
        used_percent,
        resets_at: None,
        observed_at: at.into(),
        source: QuotaSource::ResponseHeaders,
        balance: Some(balance.into()),
    }
}

fn secondary_reading(at: &str, used_percent: f64) -> QuotaReading {
    let mut reading = credit_reading(at, "0", used_percent);
    reading.window = "secondary".into();
    reading.balance = None;
    reading
}

#[test]
fn multi_timestamp_batch_never_writes_a_later_balance_into_an_earlier_row() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_data_dir(directory.path().join("data")).unwrap();
    let host = QuotaHost::new(store.clone(), Duration::ZERO).unwrap();
    host.observe(
        "codex",
        "personal",
        &[credit_reading("2026-10-21T00:00:00Z", "100.00", 0.0)],
    )
    .unwrap();
    // The secondary change is older than the credit observation in this batch.
    host.observe(
        "codex",
        "personal",
        &[
            credit_reading("2026-10-21T00:00:20Z", "90.00", 0.0),
            secondary_reading("2026-10-21T00:00:05Z", 25.0),
        ],
    )
    .unwrap();
    let rows = store.quota_history().unwrap();
    assert_eq!(rows.len(), 2);
    let secondary = &rows[1];
    assert_eq!(secondary.window, "secondary");
    assert_eq!(secondary.observed_at, "2026-10-21T00:00:05Z");
    // As of :05 the newest balance was the :00 one, not the :20 credit balance.
    assert_eq!(secondary.balance.as_deref(), Some("100.00"));
    assert_eq!(
        host.latest_balance("personal"),
        Some(("90.00".into(), "2026-10-21T00:00:20Z".into()))
    );
}

#[test]
fn changed_meters_write_rows_with_the_latest_balance_after_balance_only_updates() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_data_dir(directory.path().join("data")).unwrap();
    let host = QuotaHost::new(store.clone(), Duration::ZERO).unwrap();
    host.observe(
        "codex",
        "personal",
        &[credit_reading("2026-10-21T00:00:00Z", "100.00", 0.0)],
    )
    .unwrap();
    host.observe(
        "codex",
        "personal",
        &[secondary_reading("2026-10-21T00:00:01Z", 25.0)],
    )
    .unwrap();
    let rows_before = store.storage_stats().unwrap().catalog_transaction_count;
    // Balance-only updates: unchanged credit availability, no rows.
    host.observe(
        "codex",
        "personal",
        &[credit_reading("2026-10-21T00:00:02Z", "99.00", 0.0)],
    )
    .unwrap();
    host.observe(
        "codex",
        "personal",
        &[credit_reading("2026-10-21T00:00:03Z", "98.00", 0.0)],
    )
    .unwrap();
    assert_eq!(
        store.storage_stats().unwrap().catalog_transaction_count,
        rows_before
    );
    // A genuinely changed secondary percent writes a row with the latest balance.
    host.observe(
        "codex",
        "personal",
        &[secondary_reading("2026-10-21T00:00:04Z", 30.0)],
    )
    .unwrap();
    // Credits lose availability: a has-credits transition writes a row too.
    host.observe(
        "codex",
        "personal",
        &[credit_reading("2026-10-21T00:00:05Z", "97.00", 100.0)],
    )
    .unwrap();
    let rows = store.quota_history().unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[2].window, "secondary");
    assert_eq!(rows[2].used_percent, 30.0);
    assert_eq!(rows[2].balance.as_deref(), Some("98.00"));
    assert_eq!(rows[3].window, "credits");
    assert_eq!(rows[3].used_percent, 100.0);
    assert_eq!(rows[3].balance.as_deref(), Some("97.00"));
    assert_eq!(rows[3].observed_at, "2026-10-21T00:00:05Z");
    assert_eq!(
        host.latest_balance("personal"),
        Some(("97.00".into(), "2026-10-21T00:00:05Z".into()))
    );
}
