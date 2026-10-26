//! One retry policy and one backoff for every provider call that goes through
//! the registry. HTTP status mapping lives in `http_error`; this module decides
//! whether a classified failure may be retried and how long to wait.
use super::{ProviderError, ProviderErrorKind, cancelled};
use anyhow::Result;
use bubblepaw_core::{CancellationToken, Usage};
use std::{
    future::Future,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

pub(super) fn retryable_http_status(code: u16) -> bool {
    matches!(code, 408 | 409 | 429 | 500..=599)
}

pub(super) fn add_usage(total: &mut Option<Usage>, usage: &Usage) {
    total.get_or_insert_with(Usage::default).add(usage);
}

/// Total attempts and local backoff bounds for one provider request.
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    /// Total attempts, including the first one.
    pub max_attempts: usize,
    pub initial_delay: Duration,
    /// Caps local exponential backoff and any inline server-requested wait.
    pub max_delay: Duration,
}
impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            initial_delay: Duration::from_millis(500),
            max_delay: crate::quota::SHORT_WAIT,
        }
    }
}
impl RetryPolicy {
    /// One attempt: no transport retry at all.
    pub fn once() -> Self {
        Self {
            max_attempts: 1,
            ..Self::default()
        }
    }
}

/// Exponential backoff for a 0-based attempt, jittered into 0.5..=1.0 of its cap.
fn backoff(policy: &RetryPolicy, attempt: usize) -> Duration {
    let multiplier = 1u32.checked_shl(attempt.min(31) as u32).unwrap_or(u32::MAX);
    policy
        .initial_delay
        .saturating_mul(multiplier)
        .min(policy.max_delay)
        .mul_f64(jitter())
}

/// Jitter factor in 0.5..=1.0, from process entropy rather than a seeded RNG.
fn jitter() -> f64 {
    let entropy = uuid::Uuid::new_v4().as_u128() as u64;
    0.5 + 0.5 * (entropy as f64 / u64::MAX as f64)
}

/// The wait before retrying a failed attempt, or `None` when the failure must
/// be returned to the caller. Retries only happen before any output reached the
/// listener, and never for waits the quota host owns.
pub(crate) fn retry_delay(
    policy: &RetryPolicy,
    attempt: usize,
    error: &anyhow::Error,
    published: bool,
    cancellation: &CancellationToken,
) -> Option<Duration> {
    if attempt + 1 >= policy.max_attempts || cancellation.is_cancelled() {
        return None;
    }
    let typed = ProviderError::of(error);
    if published || typed.is_some_and(|typed| typed.published) {
        return None;
    }
    if typed.is_some_and(|typed| typed.message == "cancelled") {
        return None;
    }
    if !bubblepaw_core::provider_error_retryable(error)
        || typed.is_some_and(super::is_terminal_quota_error)
        || crate::quota::is_quota_stop(error)
    {
        return None;
    }
    let rate_limited =
        typed.is_some_and(|typed| matches!(typed.kind, ProviderErrorKind::RateLimit { .. }));
    let mut server_wait = super::provider_retry_after(error);
    if rate_limited {
        // The quota host owns rate-limit waits when it is driving the call.
        if crate::quota::host_policy_active() {
            return None;
        }
        // An exhausted window is the host's, or the caller's, decision.
        if typed.is_some_and(|typed| {
            typed
                .quota
                .iter()
                .any(|reading| reading.window != "credits" && reading.used_percent >= 100.0)
        }) {
            return None;
        }
        if let Some(ProviderErrorKind::RateLimit {
            resets_at: Some(reset),
        }) = typed.map(|typed| &typed.kind)
            && let Ok(reset) = chrono::DateTime::parse_from_rfc3339(reset)
            && let Ok(remaining) = reset.signed_duration_since(chrono::Utc::now()).to_std()
        {
            server_wait = Some(server_wait.map_or(remaining, |wait| wait.max(remaining)));
        }
    }
    if let Some(wait) = server_wait
        && wait > policy.max_delay
    {
        // Long server waits belong to the quota host or the caller.
        return None;
    }
    let backoff = backoff(policy, attempt);
    Some(server_wait.map_or(backoff, |wait| backoff.max(wait)))
}

/// Runs `call` under one policy. A failure that will be retried is reported to
/// `on_failed_attempt` so callers can keep its usage and quota readings; the
/// sleep is cancelable and returns the `cancelled` error instead of waiting.
pub(crate) async fn with_retry<T, F, Fut>(
    provider: &str,
    policy: &RetryPolicy,
    cancellation: &CancellationToken,
    published: &AtomicBool,
    mut call: F,
    mut on_failed_attempt: impl FnMut(&anyhow::Error),
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut attempt = 0;
    loop {
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }
        match call().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                let Some(delay) = retry_delay(
                    policy,
                    attempt,
                    &error,
                    published.load(Ordering::Acquire),
                    cancellation,
                ) else {
                    if attempt + 1 >= policy.max_attempts
                        && super::http_error::is_bare_bad_request(&error)
                    {
                        let mut typed = ProviderError::of(&error)
                            .expect("bare HTTP 400 keeps its typed source")
                            .clone();
                        typed.message = format!("{}; retried {attempt} times", typed.message);
                        return Err(error.context(typed));
                    }
                    return Err(error);
                };
                if super::http_error::is_bare_bad_request(&error) {
                    tracing::warn!(
                        target: "bubblepaw::provider",
                        provider,
                        status = 400,
                        attempt = attempt + 1,
                        "Retrying unstructured HTTP 400"
                    );
                }
                on_failed_attempt(&error);
                attempt += 1;
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err(cancelled()),
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::attach_retry_after;
    use super::*;
    use bubblepaw_core::{QuotaReading, QuotaSource};

    fn policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 4,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(15),
        }
    }
    fn transient() -> anyhow::Error {
        ProviderError::new(ProviderErrorKind::Transient, "server busy").into()
    }
    fn rate_limit(quota: Vec<QuotaReading>) -> anyhow::Error {
        ProviderError::new(ProviderErrorKind::RateLimit { resets_at: None }, "limited")
            .with_quota(quota)
            .into()
    }
    fn reading(window: &str, used_percent: f64) -> QuotaReading {
        QuotaReading {
            provider: "opencode".into(),
            account: "fixture".into(),
            window: window.into(),
            used_percent,
            resets_at: None,
            observed_at: String::new(),
            source: QuotaSource::Rejection,
            balance: None,
        }
    }

    #[test]
    fn published_failures_are_never_retried() {
        let token = CancellationToken::new();
        assert!(retry_delay(&policy(), 0, &transient(), false, &token).is_some());
        assert_eq!(retry_delay(&policy(), 0, &transient(), true, &token), None);
        let published: anyhow::Error = ProviderError::new(ProviderErrorKind::Transient, "cut")
            .with_published(true)
            .into();
        assert_eq!(retry_delay(&policy(), 0, &published, false, &token), None);
    }

    #[test]
    fn cancelled_calls_are_never_retried() {
        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(retry_delay(&policy(), 0, &transient(), false, &token), None);
        let cancelled: anyhow::Error =
            ProviderError::new(ProviderErrorKind::Transient, "cancelled").into();
        let live = CancellationToken::new();
        assert_eq!(retry_delay(&policy(), 0, &cancelled, false, &live), None);
    }

    #[tokio::test]
    async fn rate_limits_are_left_to_an_active_quota_host() {
        let token = CancellationToken::new();
        let limited = rate_limit(Vec::new());
        assert!(retry_delay(&policy(), 0, &limited, false, &token).is_some());
        crate::quota::with_host_policy(async {
            assert_eq!(retry_delay(&policy(), 0, &limited, false, &token), None);
        })
        .await;
    }

    #[test]
    fn exhausted_windows_are_not_retried_inline_but_credits_are_ignored() {
        let token = CancellationToken::new();
        let exhausted = rate_limit(vec![reading("five_hour", 100.0)]);
        assert_eq!(retry_delay(&policy(), 0, &exhausted, false, &token), None);
        let credits = rate_limit(vec![reading("credits", 100.0)]);
        assert!(retry_delay(&policy(), 0, &credits, false, &token).is_some());
    }

    #[test]
    fn server_waits_above_the_cap_are_not_retried_inline() {
        let token = CancellationToken::new();
        let long = attach_retry_after(transient(), 503, Some(Duration::from_secs(120)));
        assert_eq!(retry_delay(&policy(), 0, &long, false, &token), None);
    }

    #[test]
    fn server_waits_below_the_cap_are_honored() {
        let token = CancellationToken::new();
        let short = attach_retry_after(transient(), 503, Some(Duration::from_secs(2)));
        let delay = retry_delay(&policy(), 0, &short, false, &token).unwrap();
        assert!(delay >= Duration::from_secs(1), "{delay:?}");
    }

    #[test]
    fn the_last_attempt_and_one_shot_policies_are_not_retried() {
        let token = CancellationToken::new();
        assert!(retry_delay(&policy(), 2, &transient(), false, &token).is_some());
        assert_eq!(retry_delay(&policy(), 3, &transient(), false, &token), None);
        assert_eq!(
            retry_delay(&RetryPolicy::once(), 0, &transient(), false, &token),
            None
        );
    }
}
