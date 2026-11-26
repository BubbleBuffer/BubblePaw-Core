//! Provider rate-limit headers as validated quota readings. Tokens and response
//! bodies never become quota state.
pub use super::quota_events::{reading, timestamp};
use crate::{ProviderError, ProviderErrorKind, QuotaReading, QuotaSource};
use chrono::{DateTime, Duration, Utc};
use reqwest::header::HeaderMap;

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    Some(headers.get(name)?.to_str().ok()?.trim())
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

/// Retain quota evidence on any typed failure, including broken streams after
/// response headers were received. Keeps Retry-After and publication markers.
pub fn with_error_quota(error: anyhow::Error, quota: Vec<QuotaReading>) -> anyhow::Error {
    if quota.is_empty() {
        return error;
    }
    let mut typed = ProviderError::of(&error)
        .cloned()
        .unwrap_or_else(|| ProviderError::new(ProviderErrorKind::Fatal, error.to_string()));
    typed.quota.extend(quota);
    error.context(typed)
}

#[cfg(test)]
#[path = "quota_headers_tests.rs"]
mod tests;
