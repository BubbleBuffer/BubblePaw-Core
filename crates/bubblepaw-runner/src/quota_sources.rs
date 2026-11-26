//! Validated provider meters. Tokens, response bodies and credential errors never
//! become quota state; callers persist only the returned readings.
use anyhow::{Result, ensure};
pub(crate) use bubblepaw_ai::adapter::quota_headers::with_error_quota;
pub use bubblepaw_ai::adapter::quota_headers::{parse_anthropic_headers, parse_codex_headers};
use bubblepaw_ai::adapter::quota_headers::{reading, timestamp};
use bubblepaw_core::{QuotaReading, QuotaSource};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    time::Duration as StdDuration,
};
use tokio::sync::Mutex;

/// Agent SDK utilization is a fraction (0..1), unlike the usage API's percent.
/// Verified against pinned SDK 0.3.293's shipped CLI quota formatter, which
/// multiplies `utilization` by 100 for subscription warning text and windows.
/// Rejection without utilization still supplies an explicit exhausted window.
pub fn parse_claude_rate_limit(
    provider: &str,
    account: &str,
    event: &Value,
    now: DateTime<Utc>,
) -> Vec<QuotaReading> {
    let info = event.get("rate_limit_info").unwrap_or(event);
    if !matches!(
        info["status"].as_str(),
        Some("allowed" | "allowed_warning" | "rejected")
    ) {
        return Vec::new();
    }
    let mut readings = Vec::new();
    if let Some(windows) = info["unifiedWindows"].as_object() {
        for (window, meter) in windows {
            if safe_label(window)
                && let Some(used) = meter["utilization"]
                    .as_f64()
                    .filter(|value| (0.0..=1.0).contains(value))
            {
                let reset = epoch_timestamp(&meter["resetsAt"]);
                readings.extend(reading(
                    provider,
                    account,
                    window,
                    used * 100.0,
                    reset,
                    now,
                    QuotaSource::SdkEvent,
                ));
            }
        }
    }
    let rejected = info["status"] == "rejected";
    let used = info["utilization"]
        .as_f64()
        .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
        .map(|value| value * 100.0)
        .or_else(|| rejected.then_some(100.0));
    let Some(used) = used else {
        return readings;
    };
    let window = info["rateLimitType"]
        .as_str()
        .filter(|value| safe_label(value))
        .unwrap_or("subscription");
    let reset = epoch_timestamp(&info["resetsAt"]);
    readings.extend(reading(
        provider,
        account,
        window,
        used,
        reset,
        now,
        if rejected {
            QuotaSource::Rejection
        } else {
            QuotaSource::SdkEvent
        },
    ));
    readings
}
fn epoch_timestamp(value: &Value) -> Option<String> {
    value
        .as_f64()
        .filter(|value| value.is_finite() && *value >= 0.0 && *value < i64::MAX as f64)
        .and_then(|value| DateTime::from_timestamp(value as i64, 0))
        .map(|at| at.to_rfc3339())
}
fn safe_label(label: &str) -> bool {
    (1..=100).contains(&label.len())
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte))
}

pub fn parse_claude_usage(
    provider: &str,
    account: &str,
    payload: &Value,
    now: DateTime<Utc>,
) -> Vec<QuotaReading> {
    let Some(object) = payload.as_object() else {
        return Vec::new();
    };
    let mut readings: Vec<_> = object
        .iter()
        .filter_map(|(window, meter)| {
            if !safe_label(window) || !meter.is_object() {
                return None;
            }
            let used = meter["utilization"].as_f64()?;
            let reset = meter["resets_at"].as_str().and_then(timestamp);
            reading(
                provider,
                account,
                window,
                used,
                reset,
                now,
                QuotaSource::UsageApi,
            )
        })
        .collect();
    if let Some(limits) = payload["limits"].as_array() {
        for limit in limits {
            let Some(kind) = limit["kind"].as_str().filter(|kind| safe_label(kind)) else {
                continue;
            };
            let Some(used) = limit["percent"].as_f64() else {
                continue;
            };
            // The endpoint has emitted both a string scope and a model object.
            // Project only the identifying model field, never arbitrary JSON.
            let scope = limit["scope"]
                .as_str()
                .or_else(|| limit["scope"]["model"]["id"].as_str())
                .or_else(|| limit["scope"]["model"]["display_name"].as_str())
                .filter(|scope| {
                    !scope.is_empty()
                        && scope.len() <= 100
                        && scope
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b"_.:- +".contains(&byte))
                });
            let window = scope.map_or_else(|| kind.to_owned(), |scope| format!("{kind}:{scope}"));
            let reset = limit["resets_at"].as_str().and_then(timestamp);
            readings.extend(reading(
                provider,
                account,
                &window,
                used,
                reset,
                now,
                QuotaSource::UsageApi,
            ));
        }
    }
    readings
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsagePollStatus {
    Fresh,
    Floor,
    Backoff,
    CredentialsUnavailable,
    LoginExpired,
    Unauthorized,
    Ok,
}
#[derive(Debug, Clone)]
pub struct UsagePollOutcome {
    pub status: UsagePollStatus,
    pub readings: Vec<QuotaReading>,
    pub http_status: Option<u16>,
    /// What the user should do, set once when a source pauses. Never holds
    /// credentials or response text.
    pub message: Option<String>,
}
impl UsagePollOutcome {
    fn status(status: UsagePollStatus) -> Self {
        Self {
            status,
            readings: Vec::new(),
            http_status: None,
            message: None,
        }
    }
}
#[derive(Default)]
struct PollState {
    credentials_fingerprint: Option<[u8; 32]>,
    unauthorized: bool,
    last_attempt: Option<DateTime<Utc>>,
    next_attempt: Option<DateTime<Utc>>,
    last_success: Option<DateTime<Utc>>,
    failures: u32,
}
/// One shared instance per configured account. On-demand only; credential reads
/// happen even while paused, but tokens are not retained between calls.
pub struct ClaudeUsagePoller {
    provider: String,
    account: String,
    credentials_path: PathBuf,
    client: reqwest::Client,
    endpoint: String,
    state: Mutex<PollState>,
}
impl ClaudeUsagePoller {
    pub fn new(
        provider: impl Into<String>,
        account: impl Into<String>,
        credentials_path: impl AsRef<Path>,
    ) -> Result<Self> {
        let provider = provider.into();
        let account = account.into();
        ensure!(
            !provider.trim().is_empty() && !account.trim().is_empty(),
            "quota poller needs provider and account"
        );
        Ok(Self {
            provider,
            account,
            credentials_path: credentials_path.as_ref().to_owned(),
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(StdDuration::from_secs(10))
                .build()?,
            endpoint: "https://api.anthropic.com/api/oauth/usage".into(),
            state: Mutex::default(),
        })
    }
    /// `force` bypasses freshness only; a 429-triggered poll still respects the
    /// minimum cadence, backoff and authentication suspension.
    pub async fn poll_if_stale(
        &self,
        now: DateTime<Utc>,
        latest_observed_at: Option<&str>,
        force: bool,
    ) -> UsagePollOutcome {
        let mut state = self.state.lock().await;
        // Read a bounded file every time. No credentials, exception strings or
        // response errors are ever included in the outcome.
        let credentials = read_credentials(&self.credentials_path).await;
        let Some((token, expires_at, fingerprint)) = credentials else {
            return UsagePollOutcome::status(UsagePollStatus::CredentialsUnavailable);
        };
        if state.credentials_fingerprint != Some(fingerprint) {
            state.credentials_fingerprint = Some(fingerprint);
            state.unauthorized = false;
            state.failures = 0;
            state.next_attempt = None;
        }
        if expires_at <= now + Duration::seconds(60) {
            return UsagePollOutcome::status(UsagePollStatus::LoginExpired);
        }
        if state.unauthorized {
            return UsagePollOutcome::status(UsagePollStatus::Unauthorized);
        }
        if state
            .last_attempt
            .is_some_and(|last| now < last + Duration::seconds(60))
        {
            return UsagePollOutcome::status(UsagePollStatus::Floor);
        }
        if state.next_attempt.is_some_and(|next| now < next) {
            return UsagePollOutcome::status(UsagePollStatus::Backoff);
        }
        let newest = latest_observed_at
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.with_timezone(&Utc))
            .into_iter()
            .chain(state.last_success)
            .max();
        if !force && newest.is_some_and(|at| now < at + Duration::minutes(5)) {
            return UsagePollOutcome::status(UsagePollStatus::Fresh);
        }
        state.last_attempt = Some(now);
        let result = self
            .client
            .get(&self.endpoint)
            .bearer_auth(&token)
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("User-Agent", "BubblePaw/quota-1")
            .send()
            .await;
        let mut outcome = UsagePollOutcome::status(UsagePollStatus::Backoff);
        if let Ok(mut response) = result {
            outcome.http_status = Some(response.status().as_u16());
            if matches!(response.status().as_u16(), 401 | 403) {
                state.unauthorized = true;
                state.next_attempt = None;
                outcome.status = UsagePollStatus::Unauthorized;
                return outcome;
            }
            if response.status().is_success() {
                let mut body = Vec::new();
                loop {
                    match response.chunk().await {
                        Ok(Some(chunk)) if body.len() + chunk.len() <= 1_000_000 => {
                            body.extend_from_slice(&chunk)
                        }
                        Ok(None) => break,
                        _ => {
                            body.clear();
                            break;
                        }
                    }
                }
                if let Ok(payload) = serde_json::from_slice::<Value>(&body) {
                    // Response content is untrusted; only typed meter fields survive.
                    outcome.readings =
                        parse_claude_usage(&self.provider, &self.account, &payload, now);
                    outcome
                        .readings
                        .retain(|reading| !reading.window.contains(&token));
                    if !outcome.readings.is_empty() {
                        state.last_success = Some(now);
                        state.failures = 0;
                        state.next_attempt = None;
                        outcome.status = UsagePollStatus::Ok;
                        return outcome;
                    }
                }
            }
        }
        state.failures = state.failures.saturating_add(1).min(32);
        let delay = 300_i64
            .saturating_mul(1_i64 << (state.failures - 1).min(10))
            .min(1800);
        state.next_attempt = now.checked_add_signed(Duration::seconds(delay));
        outcome
    }
}

async fn read_credentials(path: &Path) -> Option<(String, DateTime<Utc>, [u8; 32])> {
    use tokio::io::AsyncReadExt;
    let file = tokio::fs::File::open(path).await.ok()?;
    let mut bytes = Vec::new();
    file.take(1_000_001).read_to_end(&mut bytes).await.ok()?;
    if bytes.len() > 1_000_000 {
        return None;
    }
    let fingerprint = Sha256::digest(&bytes).into();
    let credentials: Value = serde_json::from_slice(&bytes).ok()?;
    let oauth = &credentials["claudeAiOauth"];
    let token = oauth["accessToken"].as_str()?.to_owned();
    if token.is_empty() || token.contains(['\r', '\n']) {
        return None;
    }
    let expires_at = DateTime::from_timestamp_millis(oauth["expiresAt"].as_i64()?)?;
    Some((token, expires_at, fingerprint))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-20T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }
    #[test]
    fn sdk_success_unified_and_rejection_use_reported_window() {
        let readings = parse_claude_rate_limit(
            "claude-code",
            "a",
            &json!({"status":"allowed_warning","utilization":0.72,"rateLimitType":"five_hour","resetsAt":1792490400,"unifiedWindows":{"seven_day":{"utilization":0.13,"resetsAt":1792490400}}}),
            now(),
        );
        assert_eq!(readings.len(), 2);
        assert_eq!(readings[0].used_percent, 13.0);
        assert_eq!(readings[1].used_percent, 72.0);
        let rejection = parse_claude_rate_limit(
            "claude-code",
            "a",
            &json!({"status":"rejected","rateLimitType":"seven_day"}),
            now(),
        );
        assert_eq!(rejection[0].used_percent, 100.0);
        assert_eq!(rejection[0].source, QuotaSource::Rejection);
        assert!(rejection[0].resets_at.is_none());
        assert!(
            parse_claude_rate_limit("claude-code", "a", &json!({"status":"allowed"}), now())
                .is_empty()
        );
        // Extra usage can be disabled while the subscription request is allowed.
        // Its status alone is not evidence that any usage bucket is 100% used.
        assert!(parse_claude_rate_limit("claude-code", "a", &json!({"status":"allowed","overageStatus":"rejected","overageDisabledReason":"overage_not_provisioned"}), now()).is_empty());
    }
    #[test]
    fn usage_api_percent_is_not_sdk_fraction() {
        let readings = parse_claude_usage(
            "claude-code",
            "a",
            &json!({"five_hour":{"utilization":0.5,"resets_at":"2026-10-20T13:00:00Z"}, "seven_day":{"utilization":95.0,"resets_at":null}, "access_token":"ignored", "bad":{"utilization":101}}),
            now(),
        );
        assert_eq!(readings.len(), 2);
        assert_eq!(readings[0].used_percent, 0.5);
        assert_eq!(readings[1].used_percent, 95.0);
    }
    #[test]
    fn usage_api_limits_keep_distinct_scopes_without_arbitrary_fields() {
        let readings = parse_claude_usage(
            "claude-code",
            "a",
            &json!({"limits":[
                {"kind":"weekly_scoped","percent":42.0,"resets_at":"2026-10-24T12:00:00Z","scope":"sonnet"},
                {"kind":"weekly_scoped","percent":63.0,"resets_at":null,"scope":{"model":{"display_name":"Claude Opus"},"private":"ignored"}},
                {"kind":"weekly_scoped","percent":101,"scope":"invalid-meter"},
                {"kind":"bad space","percent":20},
                {"kind":"unknown_reset","percent":0,"resets_at":"broken","scope":"safe"}
            ]}),
            now(),
        );
        assert_eq!(readings.len(), 3);
        assert_eq!(readings[0].window, "weekly_scoped:sonnet");
        assert_eq!(readings[0].used_percent, 42.0);
        assert!(readings[0].resets_at.is_some());
        assert_eq!(readings[1].window, "weekly_scoped:Claude Opus");
        assert_eq!(readings[1].used_percent, 63.0);
        assert!(readings[1].resets_at.is_none());
        assert_eq!(readings[2].used_percent, 0.0);
        assert!(readings[2].resets_at.is_none());
    }
    fn credentials(path: &Path, token: &str, expiry: DateTime<Utc>) {
        std::fs::write(path, json!({"claudeAiOauth":{"accessToken":token,"expiresAt":expiry.timestamp_millis(),"refreshToken":"never-read-or-used"}}).to_string()).unwrap();
    }
    async fn fake_poller(
        path: &Path,
        statuses: Vec<u16>,
    ) -> (
        ClaudeUsagePoller,
        Arc<AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::{
            Router,
            http::{HeaderMap, StatusCode},
            routing::get,
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let app = Router::new().route("/api/oauth/usage", get(move |headers: HeaderMap| {
            let index = count.fetch_add(1, Ordering::SeqCst); let code = statuses[index.min(statuses.len()-1)];
            async move {
                assert!(headers["authorization"].to_str().unwrap().starts_with("Bearer synthetic-"));
                assert_eq!(headers["anthropic-beta"], "oauth-2025-04-20");
                (StatusCode::from_u16(code).unwrap(), axum::Json(json!({"five_hour":{"utilization":25,"resets_at":"2026-10-20T13:00:00Z"},"seven_day":{"utilization":50,"resets_at":"2026-10-24T12:00:00Z"}})))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut poller = ClaudeUsagePoller::new("claude-code", "a", path).unwrap();
        poller.endpoint = format!("http://{addr}/api/oauth/usage");
        (poller, calls, server)
    }
    #[tokio::test]
    async fn oauth_staleness_force_floor_and_read_only_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(".credentials.json");
        credentials(&path, "synthetic-one", now() + Duration::hours(1));
        let original = std::fs::read(&path).unwrap();
        let original_modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let (poller, calls, server) = fake_poller(&path, vec![200]).await;
        assert_eq!(
            poller
                .poll_if_stale(now(), Some(&now().to_rfc3339()), false)
                .await
                .status,
            UsagePollStatus::Fresh
        );
        let outcome = poller.poll_if_stale(now(), None, false).await;
        assert_eq!(outcome.status, UsagePollStatus::Ok);
        assert_eq!(outcome.readings.len(), 2);
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
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            original_modified
        );
        server.abort();
    }
    #[tokio::test]
    async fn oauth_unauthorized_pauses_until_credentials_change_even_forced() {
        for status in [401, 403] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join(".credentials.json");
            credentials(&path, "synthetic-one", now() + Duration::hours(1));
            let (poller, calls, server) = fake_poller(&path, vec![status, 200]).await;
            assert_eq!(
                poller.poll_if_stale(now(), None, true).await.status,
                UsagePollStatus::Unauthorized
            );
            assert_eq!(
                poller
                    .poll_if_stale(now() + Duration::minutes(10), None, true)
                    .await
                    .status,
                UsagePollStatus::Unauthorized
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            credentials(&path, "synthetic-two", now() + Duration::hours(1));
            assert_eq!(
                poller
                    .poll_if_stale(now() + Duration::minutes(10), None, true)
                    .await
                    .status,
                UsagePollStatus::Ok
            );
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            server.abort();
        }
    }
    #[tokio::test]
    async fn oauth_expired_credentials_and_failure_backoff() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(".credentials.json");
        credentials(&path, "synthetic-one", now());
        let (poller, calls, server) = fake_poller(&path, vec![429, 500, 200]).await;
        assert_eq!(
            poller.poll_if_stale(now(), None, true).await.status,
            UsagePollStatus::LoginExpired
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        credentials(&path, "synthetic-two", now() + Duration::hours(1));
        assert_eq!(
            poller.poll_if_stale(now(), None, true).await.http_status,
            Some(429)
        );
        assert_eq!(
            poller
                .poll_if_stale(now() + Duration::minutes(4), None, true)
                .await
                .status,
            UsagePollStatus::Backoff
        );
        assert_eq!(
            poller
                .poll_if_stale(now() + Duration::minutes(5), None, true)
                .await
                .http_status,
            Some(500)
        );
        assert_eq!(
            poller
                .poll_if_stale(now() + Duration::minutes(14), None, true)
                .await
                .status,
            UsagePollStatus::Backoff
        );
        assert_eq!(
            poller
                .poll_if_stale(now() + Duration::minutes(15), None, true)
                .await
                .status,
            UsagePollStatus::Ok
        );
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        server.abort();
    }
}
