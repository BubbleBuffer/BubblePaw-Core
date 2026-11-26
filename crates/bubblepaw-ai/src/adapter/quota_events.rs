//! Quota readings validated from provider data: the reading constructor shared
//! by every meter, and the Claude Agent SDK rate-limit events an extension
//! reports. Tokens and response bodies never become quota state.
use crate::{QuotaReading, QuotaSource};
use chrono::{DateTime, Utc};
use serde_json::Value;

pub(super) fn percent(value: f64) -> Option<f64> {
    (value.is_finite() && (0.0..=100.0).contains(&value)).then_some(value)
}
#[doc(hidden)]
pub fn timestamp(value: &str) -> Option<String> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|at| at.with_timezone(&Utc).to_rfc3339())
}
#[allow(clippy::too_many_arguments)]
#[doc(hidden)]
pub fn reading(
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
pub fn safe_label(label: &str) -> bool {
    (1..=100).contains(&label.len())
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte))
}
