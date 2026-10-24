//! Validated provider meters. Tokens, response bodies and credential errors never
//! become quota state; callers persist only the returned readings.
use anyhow::{Result, ensure};
use bubblepaw_core::{ProviderError, ProviderErrorKind, QuotaReading, QuotaSource};
use chrono::{DateTime, Duration, Utc};
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    time::Duration as StdDuration,
};
use tokio::sync::Mutex;

fn percent(value: f64) -> Option<f64> {
    (value.is_finite() && (0.0..=100.0).contains(&value)).then_some(value)
}
fn timestamp(value: &str) -> Option<String> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|at| at.with_timezone(&Utc).to_rfc3339())
}
fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    Some(headers.get(name)?.to_str().ok()?.trim())
}
#[allow(clippy::too_many_arguments)]
fn reading(
    provider: &str,
    account: &str,
    window: &str,
    used: f64,
    resets_at: Option<String>,
    now: DateTime<Utc>,
    source: QuotaSource,
) -> Option<QuotaReading> {
    let reading = QuotaReading {
        provider: provider.into(),
        account: account.into(),
        window: window.into(),
        used_percent: percent(used)?,
        resets_at,
        observed_at: now.to_rfc3339(),
        source,
        balance: None,
    };
    reading.validate().ok().map(|()| reading)
}

/// Anthropic API request and token buckets are independent windows. Missing or
/// malformed limit/remaining pairs are unknown, never zero or exhausted.
pub fn parse_anthropic_headers(
    provider: &str,
    account: &str,
    headers: &HeaderMap,
    now: DateTime<Utc>,
) -> Vec<QuotaReading> {
    ["requests", "tokens", "input-tokens", "output-tokens"]
        .into_iter()
        .filter_map(|window| {
            let prefix = format!("anthropic-ratelimit-{window}");
            let limit = header(headers, &format!("{prefix}-limit"))?
                .parse::<f64>()
                .ok()?;
            let remaining = header(headers, &format!("{prefix}-remaining"))?
                .parse::<f64>()
                .ok()?;
            if !limit.is_finite()
                || limit <= 0.0
                || !remaining.is_finite()
                || remaining < 0.0
                || remaining > limit
            {
                return None;
            }
            let reset = header(headers, &format!("{prefix}-reset")).and_then(timestamp);
            reading(
                provider,
                account,
                &window.replace('-', "_"),
                100.0 * (limit - remaining) / limit,
                reset,
                now,
                QuotaSource::ResponseHeaders,
            )
        })
        .collect()
}

/// Codex's official CLI parses `reset-at` (epoch seconds). Accept the unverified
/// `reset-after-seconds` compatibility spelling when no valid absolute value
/// exists. A missing, empty, zero or negative relative value is an unknown reset:
/// `resets_at` stays null and is never the observation time.
/// Primary and secondary are stable server slots, not assumed 5h/weekly buckets.
pub fn parse_codex_headers(
    provider: &str,
    account: &str,
    headers: &HeaderMap,
    now: DateTime<Utc>,
) -> Vec<QuotaReading> {
    let mut readings: Vec<_> = ["primary", "secondary"]
        .into_iter()
        .filter_map(|slot| {
            let prefix = format!("x-codex-{slot}");
            let used = header(headers, &format!("{prefix}-used-percent"))?
                .parse::<f64>()
                .ok()?;
            let reset = header(headers, &format!("{prefix}-reset-at"))
                .and_then(|raw| raw.parse::<i64>().ok())
                .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
                .or_else(|| {
                    header(headers, &format!("{prefix}-reset-after-seconds"))
                        .and_then(|raw| raw.parse::<i64>().ok())
                        .filter(|seconds| *seconds > 0)
                        .and_then(Duration::try_seconds)
                        .and_then(|delay| now.checked_add_signed(delay))
                })
                .map(|at| at.to_rfc3339());
            reading(
                provider,
                account,
                slot,
                used,
                reset,
                now,
                QuotaSource::ResponseHeaders,
            )
        })
        .collect();
    readings.extend(parse_codex_credits(provider, account, headers, now));
    readings
}

/// The Codex CLI reads credits from unprefixed headers shared by every limit
/// family. Both booleans must parse, or no credits reading is made: an absent
/// or malformed pair is unknown, never exhausted. Balance is the header as sent,
/// or `unlimited` when that flag is set.
fn parse_codex_credits(
    provider: &str,
    account: &str,
    headers: &HeaderMap,
    now: DateTime<Utc>,
) -> Option<QuotaReading> {
    let has_credits = codex_bool(headers, "x-codex-credits-has-credits")?;
    let unlimited = codex_bool(headers, "x-codex-credits-unlimited")?;
    let balance = if unlimited {
        Some("unlimited".to_owned())
    } else {
        headers
            .get("x-codex-credits-balance")
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let used = if has_credits || unlimited { 0.0 } else { 100.0 };
    let mut reading = reading(
        provider,
        account,
        "credits",
        used,
        None,
        now,
        QuotaSource::ResponseHeaders,
    )?;
    reading.balance = balance;
    Some(reading)
}

/// Same rules as the Codex CLI: `true`/`false` case-insensitively, or `1`/`0`.
fn codex_bool(headers: &HeaderMap, name: &str) -> Option<bool> {
    match headers.get(name)?.to_str().ok()? {
        raw if raw.eq_ignore_ascii_case("true") || raw == "1" => Some(true),
        raw if raw.eq_ignore_ascii_case("false") || raw == "0" => Some(false),
        _ => None,
    }
}

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

/// Retain quota evidence on any typed failure, including broken streams after
/// response headers were received. Keeps Retry-After and publication markers.
pub(crate) fn with_error_quota(error: anyhow::Error, quota: Vec<QuotaReading>) -> anyhow::Error {
    if quota.is_empty() {
        return error;
    }
    let mut typed = ProviderError::of(&error)
        .cloned()
        .unwrap_or_else(|| ProviderError::new(ProviderErrorKind::Fatal, error.to_string()));
    typed.quota.extend(quota);
    error.context(typed)
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
    fn headers(values: &[(&str, &str)]) -> HeaderMap {
        values
            .iter()
            .map(|(name, value)| {
                (
                    reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    value.parse().unwrap(),
                )
            })
            .collect()
    }
    #[test]
    fn codex_official_cli_fixture_and_older_relative_reset() {
        // https://github.com/openai/codex/blob/main/codex-rs/codex-api/src/rate_limits.rs
        let fields = headers(&[
            ("x-codex-primary-used-percent", "12.5"),
            ("x-codex-primary-window-minutes", "60"),
            ("x-codex-primary-reset-at", "1704069000"),
            ("x-codex-primary-reset-after-seconds", "999"),
            ("x-codex-secondary-used-percent", "100"),
            ("x-codex-secondary-window-minutes", "10080"),
            ("x-codex-secondary-reset-after-seconds", "120"),
        ]);
        let readings = parse_codex_headers("codex", "personal", &fields, now());
        assert_eq!(readings.len(), 2);
        assert_eq!(readings[0].window, "primary");
        assert_eq!(readings[0].used_percent, 12.5);
        assert_eq!(
            readings[0].resets_at.as_deref(),
            Some("2024-01-01T00:30:00+00:00")
        );
        assert_eq!(
            readings[1].resets_at.as_deref(),
            Some("2026-10-20T12:02:00+00:00")
        );
        assert_eq!(readings[1].source, QuotaSource::ResponseHeaders);
    }
    #[test]
    fn malformed_headers_are_unknown_and_zero_is_a_real_reading() {
        for value in ["NaN", "inf", "-1", "101", "bad"] {
            assert!(
                parse_codex_headers(
                    "codex",
                    "a",
                    &headers(&[("x-codex-primary-used-percent", value)]),
                    now()
                )
                .is_empty()
            );
        }
        assert_eq!(
            parse_codex_headers(
                "codex",
                "a",
                &headers(&[("x-codex-primary-used-percent", "0")]),
                now()
            )[0]
            .used_percent,
            0.0
        );
        assert!(parse_codex_headers("codex", "a", &HeaderMap::new(), now()).is_empty());
    }
    fn codex_primary(extra: &[(&str, &str)]) -> QuotaReading {
        let mut fields = vec![("x-codex-primary-used-percent", "40")];
        fields.extend_from_slice(extra);
        let mut readings = parse_codex_headers("codex", "a", &headers(&fields), now());
        assert_eq!(readings.len(), 1);
        let reading = readings.remove(0);
        assert_eq!(reading.observed_at, now().to_rfc3339());
        reading
    }
    #[test]
    fn codex_missing_or_unusable_relative_reset_is_unknown_not_observed() {
        let cases: &[&[(&str, &str)]] = &[
            &[],
            &[("x-codex-primary-reset-after-seconds", "")],
            &[("x-codex-primary-reset-after-seconds", "0")],
            &[("x-codex-primary-reset-after-seconds", "-5")],
            &[("x-codex-primary-reset-after-seconds", "soon")],
            &[
                ("x-codex-primary-reset-at", ""),
                ("x-codex-primary-reset-after-seconds", ""),
            ],
            &[("x-codex-primary-reset-at", "soon")],
        ];
        for case in cases {
            assert_eq!(codex_primary(case).resets_at, None, "{case:?}");
        }
    }
    #[test]
    fn codex_zero_relative_reset_does_not_borrow_secondary_or_observation_time() {
        let readings = parse_codex_headers(
            "codex",
            "a",
            &headers(&[
                ("x-codex-primary-used-percent", "10"),
                ("x-codex-primary-reset-after-seconds", "0"),
                ("x-codex-secondary-used-percent", "20"),
                ("x-codex-secondary-reset-after-seconds", "120"),
            ]),
            now(),
        );
        assert_eq!(readings.len(), 2);
        assert_eq!(readings[0].window, "primary");
        assert_eq!(readings[0].resets_at, None);
        assert_eq!(readings[1].window, "secondary");
        assert_eq!(
            readings[1].resets_at.as_deref(),
            Some("2026-10-20T12:02:00+00:00")
        );
    }
    #[test]
    fn codex_positive_relative_reset_is_fallback_when_absolute_is_unusable() {
        for absolute in ["", "soon"] {
            let reading = codex_primary(&[
                ("x-codex-primary-reset-at", absolute),
                ("x-codex-primary-reset-after-seconds", "120"),
            ]);
            assert_eq!(
                reading.resets_at.as_deref(),
                Some("2026-10-20T12:02:00+00:00"),
                "reset-at {absolute:?}"
            );
        }
    }
    #[test]
    fn codex_absolute_reset_takes_precedence_over_relative() {
        for relative in ["120", "0", "", "soon"] {
            let reading = codex_primary(&[
                ("x-codex-primary-reset-at", "1704069000"),
                ("x-codex-primary-reset-after-seconds", relative),
            ]);
            assert_eq!(
                reading.resets_at.as_deref(),
                Some("2024-01-01T00:30:00+00:00"),
                "reset-after-seconds {relative:?}"
            );
        }
        let alone = codex_primary(&[("x-codex-primary-reset-at", "1704069000")]);
        assert_eq!(
            alone.resets_at.as_deref(),
            Some("2024-01-01T00:30:00+00:00")
        );
        // Absolute epoch seconds retain their existing semantics, including
        // elapsed resets. The zero fix is specific to the relative fallback.
        assert_eq!(
            codex_primary(&[("x-codex-primary-reset-at", "0")])
                .resets_at
                .as_deref(),
            Some("1970-01-01T00:00:00+00:00")
        );
    }
    fn codex_credits(extra: &[(&str, &str)]) -> Vec<QuotaReading> {
        parse_codex_headers("codex", "a", &headers(extra), now())
    }
    #[test]
    fn codex_credits_available_and_unavailable_use_has_credits() {
        let available = codex_credits(&[
            ("x-codex-credits-has-credits", "true"),
            ("x-codex-credits-unlimited", "false"),
            ("x-codex-credits-balance", "12.50"),
        ]);
        assert_eq!(available.len(), 1);
        assert_eq!(available[0].window, "credits");
        assert_eq!(available[0].used_percent, 0.0);
        assert_eq!(available[0].resets_at, None);
        assert_eq!(available[0].balance.as_deref(), Some("12.50"));
        assert_eq!(available[0].source, QuotaSource::ResponseHeaders);
        assert_eq!(available[0].observed_at, now().to_rfc3339());
        let unavailable = codex_credits(&[
            ("x-codex-credits-has-credits", "false"),
            ("x-codex-credits-unlimited", "0"),
            ("x-codex-credits-balance", "0.00"),
        ]);
        assert_eq!(unavailable.len(), 1);
        assert_eq!(unavailable[0].used_percent, 100.0);
        assert_eq!(unavailable[0].resets_at, None);
        assert_eq!(unavailable[0].balance.as_deref(), Some("0.00"));
        let verbatim = codex_credits(&[
            ("x-codex-credits-has-credits", "true"),
            ("x-codex-credits-unlimited", "false"),
            ("x-codex-credits-balance", " 0012.500 "),
        ]);
        assert_eq!(verbatim[0].balance.as_deref(), Some(" 0012.500 "));
    }
    #[test]
    fn codex_credits_unlimited_reports_available_with_unlimited_balance() {
        let unlimited = codex_credits(&[
            ("x-codex-credits-has-credits", "FALSE"),
            ("x-codex-credits-unlimited", "True"),
            ("x-codex-credits-balance", "7"),
        ]);
        assert_eq!(unlimited[0].used_percent, 0.0);
        assert_eq!(unlimited[0].balance.as_deref(), Some("unlimited"));
        let both = codex_credits(&[
            ("x-codex-credits-has-credits", "1"),
            ("x-codex-credits-unlimited", "1"),
            ("x-codex-credits-balance", "5"),
        ]);
        assert_eq!(both[0].used_percent, 0.0);
        assert_eq!(both[0].balance.as_deref(), Some("unlimited"));
        let without_balance = codex_credits(&[
            ("x-codex-credits-has-credits", "0"),
            ("x-codex-credits-unlimited", "1"),
        ]);
        assert_eq!(without_balance[0].used_percent, 0.0);
        assert_eq!(without_balance[0].balance.as_deref(), Some("unlimited"));
    }
    #[test]
    fn codex_credits_balance_is_absent_when_header_is_missing_or_empty() {
        for balance in [None, Some("")] {
            let mut fields = vec![
                ("x-codex-credits-has-credits", "true"),
                ("x-codex-credits-unlimited", "false"),
            ];
            if let Some(value) = balance {
                fields.push(("x-codex-credits-balance", value));
            }
            let readings = codex_credits(&fields);
            assert_eq!(readings.len(), 1);
            assert_eq!(readings[0].balance, None, "{balance:?}");
            assert_eq!(readings[0].used_percent, 0.0);
        }
    }
    #[test]
    fn codex_credits_partial_or_malformed_pairs_establish_no_reading() {
        for fields in [
            &[("x-codex-credits-has-credits", "false")][..],
            &[
                ("x-codex-credits-unlimited", "false"),
                ("x-codex-credits-balance", "0"),
            ][..],
            &[
                ("x-codex-credits-has-credits", "yes"),
                ("x-codex-credits-unlimited", "false"),
            ][..],
            &[
                ("x-codex-credits-has-credits", "true"),
                ("x-codex-credits-unlimited", "maybe"),
            ][..],
            &[
                ("x-codex-credits-has-credits", ""),
                ("x-codex-credits-unlimited", "false"),
            ][..],
            &[
                ("x-codex-credits-has-credits", "2"),
                ("x-codex-credits-unlimited", "0"),
            ][..],
        ] {
            assert!(codex_credits(fields).is_empty(), "{fields:?}");
        }
    }
    #[test]
    fn codex_credits_append_after_windows_and_never_remove_them() {
        let readings = codex_credits(&[
            ("x-codex-primary-used-percent", "40"),
            ("x-codex-credits-has-credits", "true"),
            ("x-codex-credits-unlimited", "false"),
            ("x-codex-credits-balance", "3"),
        ]);
        assert_eq!(
            readings
                .iter()
                .map(|reading| reading.window.as_str())
                .collect::<Vec<_>>(),
            ["primary", "credits"]
        );
        assert_eq!(readings[0].balance, None);
        assert_eq!(readings[1].balance.as_deref(), Some("3"));
        let partial = codex_credits(&[
            ("x-codex-primary-used-percent", "40"),
            ("x-codex-credits-has-credits", "maybe"),
            ("x-codex-credits-unlimited", "false"),
        ]);
        assert_eq!(partial.len(), 1);
        assert_eq!(partial[0].window, "primary");
    }
    #[test]
    fn anthropic_independent_buckets_validate_pairs() {
        let mut fields = HeaderMap::new();
        for bucket in ["requests", "tokens", "input-tokens", "output-tokens"] {
            fields.extend(headers(&[
                (&format!("anthropic-ratelimit-{bucket}-limit"), "200"),
                (&format!("anthropic-ratelimit-{bucket}-remaining"), "50"),
                (
                    &format!("anthropic-ratelimit-{bucket}-reset"),
                    "2026-10-20T12:01:00Z",
                ),
            ]));
        }
        let readings = parse_anthropic_headers("anthropic", "a", &fields, now());
        assert_eq!(readings.len(), 4);
        assert_eq!(
            readings
                .iter()
                .map(|reading| reading.window.as_str())
                .collect::<Vec<_>>(),
            ["requests", "tokens", "input_tokens", "output_tokens"]
        );
        assert!(
            readings
                .iter()
                .all(|reading| reading.used_percent == 75.0 && reading.validate().is_ok())
        );
        for (limit, remaining) in [("0", "0"), ("NaN", "1"), ("10", "11"), ("10", "-1")] {
            assert!(
                parse_anthropic_headers(
                    "anthropic",
                    "a",
                    &headers(&[
                        ("anthropic-ratelimit-tokens-limit", limit),
                        ("anthropic-ratelimit-tokens-remaining", remaining)
                    ]),
                    now()
                )
                .is_empty()
            );
        }
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
