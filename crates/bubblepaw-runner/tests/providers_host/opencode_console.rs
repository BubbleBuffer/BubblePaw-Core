//! OpenCode Console login and Go quota against a local fake console. No live
//! calls: every request goes to 127.0.0.1.
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use bubblepaw_core::{
    CancellationToken, ModelResponse, ProviderError, ProviderErrorKind, QuotaReading, QuotaSource,
    Store,
};
use bubblepaw_runner::{
    opencode_console::{
        ConsoleClient, ConsoleCredential, OpenCodeUsagePoller, Sleeper, credential_path,
        delete_credential, parse_go_status, read_credential, write_credential,
    },
    quota::{LimitAction, QuotaHost, classify_limit},
    quota_sources::UsagePollStatus,
};
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration as StdDuration,
};

#[derive(Default)]
struct Fake {
    token_replies: Mutex<VecDeque<(u16, Value)>>,
    token_requests: Mutex<Vec<Value>>,
    go_codes: Mutex<VecDeque<u16>>,
    go_body: Mutex<Value>,
    go_tokens: Mutex<Vec<String>>,
    go_calls: AtomicUsize,
    billing_calls: AtomicUsize,
}

fn check_headers(headers: &HeaderMap) -> String {
    let agent = headers["user-agent"].to_str().unwrap();
    assert!(agent.starts_with("bubblepaw/"), "{agent}");
    assert_eq!(headers["x-org-id"], "org_1");
    headers["authorization"]
        .to_str()
        .unwrap()
        .strip_prefix("Bearer ")
        .unwrap()
        .to_owned()
}

async fn serve(fake: Arc<Fake>) -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new()
        .route(
            "/console/auth/device/code",
            post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                assert!(headers["user-agent"].to_str().unwrap().starts_with("bubblepaw/"));
                assert_eq!(body, json!({"client_id":"opencode-cli","supports_org_scope":true}));
                Json(json!({"device_code":"dev-secret-code","user_code":"ABCD-EFGH","verification_uri_complete":"device?code=ABCD-EFGH","interval":5,"expires_in":600}))
            }),
        )
        .route(
            "/console/auth/device/token",
            post(|State(fake): State<Arc<Fake>>, Json(body): Json<Value>| async move {
                fake.token_requests.lock().unwrap().push(body);
                let (code, reply) = fake
                    .token_replies
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or((500, json!({})));
                (StatusCode::from_u16(code).unwrap(), Json(reply))
            }),
        )
        .route(
            "/console/api/go/status",
            get(|State(fake): State<Arc<Fake>>, headers: HeaderMap| async move {
                fake.go_calls.fetch_add(1, Ordering::SeqCst);
                fake.go_tokens.lock().unwrap().push(check_headers(&headers));
                let code = fake.go_codes.lock().unwrap().pop_front().unwrap_or(200);
                let body = fake.go_body.lock().unwrap().clone();
                (StatusCode::from_u16(code).unwrap(), Json(body))
            }),
        )
        .route(
            "/console/api/billing/status",
            get(|State(fake): State<Arc<Fake>>, headers: HeaderMap| async move {
                fake.billing_calls.fetch_add(1, Ordering::SeqCst);
                check_headers(&headers);
                Json(json!({"billingMode":"prepaid","balanceMicroCents":"1032160675","availableMicroCents":"1032160675","creditLimitMicroCents":null}))
            }),
        )
        .with_state(fake);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}/console/"), server)
}

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-21T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn go_status(five_hour_used: &str, resets_at: &str, use_balance: bool) -> Value {
    json!({"product":"go-plus","useBalance":use_balance,"access":{"startsAt":"2026-10-01T00:00:00Z","endsAt":"2026-11-01T00:00:00Z","meters":{
        "fiveHour":{"startsAt":"2026-10-21T10:00:00Z","resetsAt":resets_at,"limitMicroCents":"4800000000","usedMicroCents":five_hour_used},
        "week":{"startsAt":"2026-10-19T00:00:00Z","resetsAt":"2026-10-26T00:00:00Z","limitMicroCents":"12000000000","usedMicroCents":"3000000000"},
        "month":{"resetsAt":"2026-11-01T00:00:00Z","limitMicroCents":"24000000000","usedMicroCents":"0"}}}})
}

fn recording_sleeper() -> (Sleeper, Arc<Mutex<Vec<StdDuration>>>) {
    let sleeps = Arc::new(Mutex::new(Vec::new()));
    let record = sleeps.clone();
    let sleeper: Sleeper = Arc::new(
        move |delay| -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
            record.lock().unwrap().push(delay);
            Box::pin(async {})
        },
    );
    (sleeper, sleeps)
}

fn granted(access: &str, refresh: &str) -> (u16, Value) {
    (
        200,
        json!({"access_token":access,"refresh_token":refresh,"expires_in":2_592_000,"org_id":"org_1","token_type":"Bearer"}),
    )
}

fn pending(error: &str) -> (u16, Value) {
    (400, json!({"error":error}))
}

async fn login(
    replies: Vec<(u16, Value)>,
) -> (anyhow::Result<ConsoleCredential>, String, Vec<u64>) {
    let fake = Arc::new(Fake::default());
    fake.token_replies.lock().unwrap().extend(replies);
    let (base, server) = serve(fake.clone()).await;
    let (sleeper, sleeps) = recording_sleeper();
    let mut out = Vec::new();
    let result = ConsoleClient::with_base_url(&base)
        .unwrap()
        .device_login(&mut out, &sleeper)
        .await;
    server.abort();
    for request in fake.token_requests.lock().unwrap().iter() {
        assert_eq!(
            request["grant_type"],
            "urn:ietf:params:oauth:grant-type:device_code"
        );
        assert_eq!(request["device_code"], "dev-secret-code");
        assert_eq!(request["client_id"], "opencode-cli");
    }
    let sleeps = sleeps.lock().unwrap().iter().map(|d| d.as_secs()).collect();
    (result, String::from_utf8(out).unwrap(), sleeps)
}

#[tokio::test]
async fn device_login_success_prints_link_and_code_only() {
    let (result, out, sleeps) = login(vec![granted("acc-secret-1", "ref-secret-1")]).await;
    let credential = result.unwrap();
    assert_eq!(credential.access_token, "acc-secret-1");
    assert_eq!(credential.refresh_token, "ref-secret-1");
    assert_eq!(credential.org_id, "org_1");
    assert!(credential.expires_at > Utc::now() + Duration::days(29));
    assert!(out.contains("/console/device?code=ABCD-EFGH"), "{out}");
    assert!(out.contains("ABCD-EFGH"));
    assert_eq!(sleeps, [5]);
    let debug = format!("{credential:?}");
    for secret in ["acc-secret-1", "ref-secret-1", "dev-secret-code"] {
        assert!(!out.contains(secret));
        assert!(!debug.contains(secret));
    }
}

#[tokio::test]
async fn device_login_pending_then_success() {
    let (result, out, sleeps) = login(vec![
        pending("authorization_pending"),
        pending("authorization_pending"),
        granted("acc-secret-2", "ref-secret-2"),
    ])
    .await;
    assert_eq!(result.unwrap().access_token, "acc-secret-2");
    assert_eq!(sleeps, [5, 5, 5]);
    assert!(!out.contains("acc-secret-2") && !out.contains("ref-secret-2"));
}

#[tokio::test]
async fn device_login_slow_down_adds_five_seconds() {
    let (result, _, sleeps) = login(vec![
        pending("slow_down"),
        pending("authorization_pending"),
        pending("slow_down"),
        granted("acc-secret-3", "ref-secret-3"),
    ])
    .await;
    assert!(result.is_ok());
    assert_eq!(sleeps, [5, 10, 10, 15]);
}

#[tokio::test]
async fn device_login_expired_code_asks_to_rerun() {
    let (result, out, _) = login(vec![
        pending("authorization_pending"),
        pending("expired_token"),
    ])
    .await;
    let error = format!("{:#}", result.unwrap_err());
    assert!(error.contains("bubblepaw login opencode"), "{error}");
    assert!(!error.contains("dev-secret-code") && !out.contains("dev-secret-code"));
}

fn credential(access: &str, refresh: &str, expires_at: DateTime<Utc>) -> ConsoleCredential {
    ConsoleCredential {
        access_token: access.into(),
        refresh_token: refresh.into(),
        expires_at,
        org_id: "org_1".into(),
    }
}

#[test]
fn credential_file_is_atomic_user_only_and_removable() {
    let directory = tempfile::tempdir().unwrap();
    let path = credential_path(directory.path(), "opencode").unwrap();
    assert!(path.ends_with("auth/opencode-console-opencode.json"));
    assert!(credential_path(directory.path(), "../x").is_err());
    assert!(read_credential(&path).is_none());
    let stored = credential("acc-a", "ref-a", now());
    write_credential(&path, &stored).unwrap();
    assert_eq!(read_credential(&path).unwrap().0, stored);
    write_credential(&path, &credential("acc-b", "ref-b", now())).unwrap();
    assert_eq!(read_credential(&path).unwrap().0.access_token, "acc-b");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // Only the credential remains in the auth directory: no temporary files.
    assert_eq!(
        std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
        1
    );
    assert!(delete_credential(&path).unwrap());
    assert!(!delete_credential(&path).unwrap());
}

#[test]
fn go_status_meters_and_credits_parse_from_verified_shapes() {
    let readings = parse_go_status(
        "opencode",
        "opencode",
        &go_status("53819227", "2026-10-21T15:00:00Z", true),
        Some(
            &json!({"billingMode":"prepaid","balanceMicroCents":"1032160675","availableMicroCents":"1032160675","creditLimitMicroCents":null}),
        ),
        now(),
    );
    let windows: Vec<_> = readings.iter().map(|r| r.window.as_str()).collect();
    assert_eq!(windows, ["five_hour", "week", "month", "credits"]);
    assert!((readings[0].used_percent - 100.0 * 53_819_227.0 / 4_800_000_000.0).abs() < 1e-9);
    assert_eq!(
        readings[0].resets_at.as_deref(),
        Some("2026-10-21T15:00:00+00:00")
    );
    assert_eq!(readings[1].used_percent, 25.0);
    assert_eq!(readings[2].used_percent, 0.0);
    assert!(readings.iter().all(|r| r.source == QuotaSource::UsageApi));
    assert_eq!(readings[3].used_percent, 0.0);
    assert_eq!(readings[3].resets_at, None);
    assert_eq!(readings[3].balance.as_deref(), Some("10.32"));
    // No balance use or nothing available: credits are not available.
    let off = parse_go_status(
        "opencode",
        "opencode",
        &go_status("0", "2026-10-21T15:00:00Z", false),
        Some(&json!({"availableMicroCents":"500","balanceMicroCents":"500"})),
        now(),
    );
    assert_eq!(off[3].used_percent, 100.0);
    assert_eq!(off[3].balance.as_deref(), Some("0.00"));
    // Missing billing or malformed meters are unknown, never exhausted.
    let partial = parse_go_status(
        "opencode",
        "opencode",
        &json!({"access":{"meters":{"fiveHour":{"limitMicroCents":"0","usedMicroCents":"1"},"week":{"limitMicroCents":"x"}}}}),
        None,
        now(),
    );
    assert!(partial.is_empty());
}

struct Setup {
    fake: Arc<Fake>,
    server: tokio::task::JoinHandle<()>,
    poller: OpenCodeUsagePoller,
    _directory: tempfile::TempDir,
    path: std::path::PathBuf,
}

async fn setup(stored: ConsoleCredential) -> Setup {
    let directory = tempfile::tempdir().unwrap();
    let path = credential_path(directory.path(), "opencode").unwrap();
    write_credential(&path, &stored).unwrap();
    let fake = Arc::new(Fake::default());
    *fake.go_body.lock().unwrap() = go_status("53819227", "2026-10-21T15:00:00Z", true);
    let (base, server) = serve(fake.clone()).await;
    let poller = OpenCodeUsagePoller::new(
        "opencode",
        "opencode",
        &path,
        ConsoleClient::with_base_url(&base).unwrap(),
    )
    .unwrap();
    Setup {
        fake,
        server,
        poller,
        _directory: directory,
        path,
    }
}

fn stored_tokens(path: &Path) -> (String, String) {
    let (credential, _) = read_credential(path).unwrap();
    (credential.access_token, credential.refresh_token)
}

#[tokio::test]
async fn no_login_means_no_source_and_no_request() {
    let directory = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let (base, server) = serve(fake.clone()).await;
    let poller = OpenCodeUsagePoller::new(
        "opencode",
        "opencode",
        credential_path(directory.path(), "opencode").unwrap(),
        ConsoleClient::with_base_url(&base).unwrap(),
    )
    .unwrap();
    let outcome = poller.poll_if_stale(now(), None, true).await;
    assert_eq!(outcome.status, UsagePollStatus::CredentialsUnavailable);
    assert!(outcome.message.is_none() && outcome.readings.is_empty());
    assert_eq!(fake.go_calls.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test]
async fn polls_on_demand_with_freshness_and_sixty_second_floor() {
    let setup = setup(credential(
        "acc-live",
        "ref-live",
        now() + Duration::days(20),
    ))
    .await;
    let poller = &setup.poller;
    assert_eq!(
        poller
            .poll_if_stale(now(), Some(&now().to_rfc3339()), false)
            .await
            .status,
        UsagePollStatus::Fresh
    );
    let outcome = poller.poll_if_stale(now(), None, false).await;
    assert_eq!(outcome.status, UsagePollStatus::Ok);
    assert_eq!(outcome.readings.len(), 4);
    assert_eq!(
        poller
            .poll_if_stale(now() + Duration::seconds(59), None, true)
            .await
            .status,
        UsagePollStatus::Floor
    );
    assert_eq!(
        poller
            .poll_if_stale(now() + Duration::seconds(60), None, false)
            .await
            .status,
        UsagePollStatus::Fresh
    );
    assert_eq!(
        poller
            .poll_if_stale(now() + Duration::seconds(60), None, true)
            .await
            .status,
        UsagePollStatus::Ok
    );
    assert_eq!(setup.fake.go_calls.load(Ordering::SeqCst), 2);
    assert_eq!(setup.fake.billing_calls.load(Ordering::SeqCst), 2);
    assert!(setup.fake.token_requests.lock().unwrap().is_empty());
    setup.server.abort();
}

#[tokio::test]
async fn expired_access_token_is_refreshed_and_rotation_written_back() {
    let setup = setup(credential("acc-old", "ref-old", now())).await;
    setup
        .fake
        .token_replies
        .lock()
        .unwrap()
        .push_back(granted("acc-new", "ref-new"));
    let outcome = setup.poller.poll_if_stale(now(), None, true).await;
    assert_eq!(outcome.status, UsagePollStatus::Ok);
    let requests = setup.fake.token_requests.lock().unwrap().clone();
    assert_eq!(
        requests,
        [
            json!({"grant_type":"refresh_token","refresh_token":"ref-old","client_id":"opencode-cli"})
        ]
    );
    assert_eq!(*setup.fake.go_tokens.lock().unwrap(), ["acc-new"]);
    assert_eq!(
        stored_tokens(&setup.path),
        ("acc-new".to_owned(), "ref-new".to_owned())
    );
    for reading in &outcome.readings {
        let text = serde_json::to_string(reading).unwrap();
        assert!(!text.contains("acc-") && !text.contains("ref-"));
    }
    setup.server.abort();
}

#[tokio::test]
async fn unauthorized_status_refreshes_once_then_retries() {
    let setup = setup(credential("acc-old", "ref-old", now() + Duration::days(20))).await;
    setup.fake.go_codes.lock().unwrap().push_back(401);
    setup
        .fake
        .token_replies
        .lock()
        .unwrap()
        .push_back(granted("acc-new", "ref-new"));
    let outcome = setup.poller.poll_if_stale(now(), None, true).await;
    assert_eq!(outcome.status, UsagePollStatus::Ok);
    assert_eq!(
        *setup.fake.go_tokens.lock().unwrap(),
        ["acc-old", "acc-new"]
    );
    assert_eq!(stored_tokens(&setup.path).1, "ref-new");
    // A refreshed token that is refused too pauses without looping.
    setup.fake.go_codes.lock().unwrap().extend([401, 401]);
    setup
        .fake
        .token_replies
        .lock()
        .unwrap()
        .push_back(granted("acc-third", "ref-third"));
    let later = now() + Duration::minutes(10);
    let outcome = setup.poller.poll_if_stale(later, None, true).await;
    assert_eq!(outcome.status, UsagePollStatus::Unauthorized);
    assert!(
        outcome
            .message
            .unwrap()
            .contains("bubblepaw login opencode")
    );
    assert_eq!(
        setup
            .poller
            .poll_if_stale(later + Duration::minutes(10), None, true)
            .await
            .status,
        UsagePollStatus::Unauthorized
    );
    assert_eq!(setup.fake.token_requests.lock().unwrap().len(), 2);
    setup.server.abort();
}

#[tokio::test]
async fn invalid_grant_pauses_until_a_new_login() {
    let setup = setup(credential("acc-old", "ref-old", now())).await;
    setup
        .fake
        .token_replies
        .lock()
        .unwrap()
        .push_back((400, json!({"error":"invalid_grant"})));
    let outcome = setup.poller.poll_if_stale(now(), None, true).await;
    assert_eq!(outcome.status, UsagePollStatus::LoginExpired);
    let message = outcome.message.unwrap();
    assert!(message.contains("bubblepaw login opencode"), "{message}");
    assert!(!message.contains("acc-old") && !message.contains("ref-old"));
    let paused = setup
        .poller
        .poll_if_stale(now() + Duration::hours(1), None, true)
        .await;
    assert_eq!(paused.status, UsagePollStatus::LoginExpired);
    assert!(paused.message.is_none());
    assert_eq!(setup.fake.token_requests.lock().unwrap().len(), 1);
    assert_eq!(setup.fake.go_calls.load(Ordering::SeqCst), 0);
    // A fresh login (the file changes) resumes the source.
    write_credential(
        &setup.path,
        &credential("acc-login", "ref-login", now() + Duration::days(30)),
    )
    .unwrap();
    assert_eq!(
        setup
            .poller
            .poll_if_stale(now() + Duration::hours(1), None, true)
            .await
            .status,
        UsagePollStatus::Ok
    );
    setup.server.abort();
}

#[tokio::test]
async fn failures_back_off() {
    let setup = setup(credential(
        "acc-live",
        "ref-live",
        now() + Duration::days(20),
    ))
    .await;
    setup.fake.go_codes.lock().unwrap().extend([500, 429]);
    assert_eq!(
        setup.poller.poll_if_stale(now(), None, true).await.status,
        UsagePollStatus::Backoff
    );
    assert_eq!(
        setup
            .poller
            .poll_if_stale(now() + Duration::minutes(4), None, true)
            .await
            .status,
        UsagePollStatus::Backoff
    );
    assert_eq!(setup.fake.go_calls.load(Ordering::SeqCst), 1);
    setup.server.abort();
}

#[tokio::test]
async fn unchanged_meters_add_no_history_rows() {
    let setup = setup(credential(
        "acc-live",
        "ref-live",
        now() + Duration::days(20),
    ))
    .await;
    let store = Store::open_data_dir(setup._directory.path().join("data")).unwrap();
    let host = QuotaHost::new(store.clone(), StdDuration::ZERO).unwrap();
    for minutes in [0, 6, 12] {
        let outcome = setup
            .poller
            .poll_if_stale(now() + Duration::minutes(minutes), None, false)
            .await;
        assert_eq!(outcome.status, UsagePollStatus::Ok);
        host.observe("opencode", "opencode", &outcome.readings)
            .unwrap();
    }
    assert_eq!(store.quota_history().unwrap().len(), 4);
    *setup.fake.go_body.lock().unwrap() = go_status("4800000000", "2026-10-21T15:00:00Z", true);
    let outcome = setup
        .poller
        .poll_if_stale(now() + Duration::minutes(18), None, false)
        .await;
    host.observe("opencode", "opencode", &outcome.readings)
        .unwrap();
    assert_eq!(store.quota_history().unwrap().len(), 5);
    setup.server.abort();
}

fn opencode_reading(window: &str, used: f64, reset: Option<DateTime<Utc>>) -> QuotaReading {
    QuotaReading {
        provider: "opencode".into(),
        account: "opencode".into(),
        window: window.into(),
        used_percent: used,
        resets_at: reset.map(|at| at.to_rfc3339()),
        observed_at: Utc::now().to_rfc3339(),
        source: QuotaSource::UsageApi,
        balance: None,
    }
}

fn rate_limited() -> anyhow::Error {
    ProviderError::new(
        ProviderErrorKind::RateLimit { resets_at: None },
        "request rejected",
    )
    .into()
}

#[test]
fn opencode_windows_are_account_wide_and_credits_never_exhaust() {
    let now = Utc::now();
    let reset = now + Duration::hours(2);
    for window in ["five_hour", "week", "month"] {
        for model in ["deepseek-v4.1-flash", "glm-5", "kimi-k3"] {
            assert_eq!(
                classify_limit(
                    "opencode",
                    model,
                    &rate_limited(),
                    &[opencode_reading(window, 100.0, Some(reset))],
                    now,
                    StdDuration::from_secs(86_400),
                ),
                LimitAction::Park {
                    resume_at: reset,
                    window: window.into()
                },
                "{window} {model}"
            );
        }
    }
    let mut credits = opencode_reading("credits", 100.0, None);
    credits.balance = Some("0.00".into());
    assert!(matches!(
        classify_limit(
            "opencode",
            "glm-5",
            &rate_limited(),
            &[credits, opencode_reading("week", 40.0, Some(reset))],
            now,
            StdDuration::from_secs(86_400),
        ),
        LimitAction::Short(_)
    ));
}

#[tokio::test]
async fn opencode_429_with_exhausted_five_hour_parks_until_reset() {
    let directory = tempfile::tempdir().unwrap();
    let path = credential_path(directory.path(), "opencode").unwrap();
    write_credential(
        &path,
        &credential("acc-live", "ref-live", Utc::now() + Duration::days(20)),
    )
    .unwrap();
    let fake = Arc::new(Fake::default());
    let reset = Utc::now() + Duration::milliseconds(400);
    // Five-hour exhausted, credits unavailable (no reset): only five_hour decides.
    *fake.go_body.lock().unwrap() = go_status("4800000000", &reset.to_rfc3339(), false);
    let (base, server) = serve(fake.clone()).await;
    let poller = OpenCodeUsagePoller::new(
        "opencode",
        "opencode",
        &path,
        ConsoleClient::with_base_url(&base).unwrap(),
    )
    .unwrap();
    let store = Store::open_data_dir(directory.path().join("data")).unwrap();
    let host = QuotaHost::new(store, StdDuration::from_secs(60))
        .unwrap()
        .with_opencode_poller(poller);
    let calls = AtomicUsize::new(0);
    let started = std::time::Instant::now();
    let response = host
        .execute(
            "opencode",
            "opencode",
            "deepseek-v4.1-flash",
            CancellationToken::new(),
            || {
                let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
                async move {
                    if first {
                        Err(rate_limited())
                    } else {
                        Ok(ModelResponse {
                            text: "ok".into(),
                            tool_calls: Vec::new(),
                            usage: None,
                            quota: Vec::new(),
                            last_request_usage: None,
                            provider_state: None,
                        })
                    }
                }
            },
        )
        .await
        .unwrap();
    assert_eq!(response.text, "ok");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(Utc::now() >= reset);
    assert!(started.elapsed() < StdDuration::from_secs(10));
    // One console poll before the request; the post-429 poll hits the floor.
    assert_eq!(fake.go_calls.load(Ordering::SeqCst), 1);
    let latest = host.latest("opencode");
    assert!(
        latest
            .iter()
            .any(|r| r.window == "five_hour" && r.used_percent == 100.0)
    );
    assert!(
        latest
            .iter()
            .any(|r| r.window == "credits" && r.used_percent == 100.0)
    );
    server.abort();
}
