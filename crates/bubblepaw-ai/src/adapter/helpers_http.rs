//! Helpers of the HTTP adapters: publication tracking, retained usage and quota
//! across continuation rounds, Retry-After parsing and the Codex auth file.
use super::{RejectionQuotaBoundary, classify, current_rejection_quota_count, with_error_quota};
use crate::{
    ProviderError, ProviderErrorKind, ReasoningEffort, StreamCallback, StreamEvent, Usage,
};
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// Publication follows listener delivery, including prior continuation rounds.
pub fn track_text_publication(on_text: StreamCallback) -> (StreamCallback, Arc<AtomicBool>) {
    let published = Arc::new(AtomicBool::new(false));
    let signal = published.clone();
    let listener: StreamCallback = Arc::new(move |event| {
        if matches!(event, StreamEvent::TextDelta(text) if !text.is_empty()) {
            signal.store(true, Ordering::Release);
        }
        on_text(event);
    });
    (listener, published)
}

/// Keep accepted physical continuation usage when a later response fails.
pub(super) fn retain_prior_usage(error: anyhow::Error, prior: Option<&Usage>) -> anyhow::Error {
    let Some(prior) = prior else {
        return error;
    };
    let typed = ProviderError::of(&error)
        .cloned()
        .unwrap_or_else(|| ProviderError::new(ProviderErrorKind::Fatal, error.to_string()));
    let mut total = prior.clone();
    if let Some(usage) = &typed.usage {
        total.add(usage);
    }
    error.context(typed.with_usage(total))
}

/// Preserve the error chain so usage and provider Retry-After remain available.
pub fn mark_published(error: anyhow::Error, published: bool) -> anyhow::Error {
    if !published {
        return error;
    }
    let typed = ProviderError::of(&error)
        .cloned()
        .unwrap_or_else(|| ProviderError::new(ProviderErrorKind::Fatal, error.to_string()));
    if typed.published {
        error
    } else {
        error.context(typed.with_published(true))
    }
}

/// Parse Retry-After delta-seconds or an HTTP date. Past dates mean no delay.
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value.parse::<u64>().ok().map(Duration::from_secs);
    }
    httpdate::parse_http_date(value).ok().map(|deadline| {
        deadline
            .duration_since(std::time::SystemTime::now())
            .unwrap_or_default()
    })
}

/// Keep all observations, with a stable boundary around the current rejection.
/// Earlier retry/continuation observations cannot name its rejecting window.
pub fn retain_prior_quota(error: anyhow::Error, quota: Vec<crate::QuotaReading>) -> anyhow::Error {
    let current_count = if current_rejection_quota_count(&error).is_none() {
        ProviderError::of(&error)
            .filter(|typed| matches!(typed.kind, ProviderErrorKind::RateLimit { .. }))
            .map(|typed| typed.quota.len())
    } else {
        None
    };
    let error = with_error_quota(error, quota);
    match current_count {
        Some(count) => {
            let message = error.to_string();
            error.context(RejectionQuotaBoundary { count, message })
        }
        None => error,
    }
}

pub(super) fn parse_reasoning_effort(effort: &str) -> Result<ReasoningEffort> {
    serde_json::from_value(json!(effort)).map_err(|_| {
        anyhow!("Unsupported reasoning effort; expected low, medium, high, xhigh or max")
    })
}

pub(super) fn transient(error: anyhow::Error) -> anyhow::Error {
    classify(ProviderErrorKind::Transient, error)
}

pub(super) fn is_context_overflow(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("context_length_exceeded")
        || lower.contains("maximum context length")
        || lower.contains("context window")
}

pub(super) fn read_codex(path: &Path) -> Result<(String, String)> {
    let auth_error = |message: &str| {
        classify(
            ProviderErrorKind::Auth,
            anyhow!("{message}: {}", path.display()),
        )
    };
    let bytes = std::fs::read(path).map_err(|_| auth_error("cannot read Codex auth file"))?;
    let auth: Value = serde_json::from_slice(&bytes)
        .map_err(|_| auth_error("invalid JSON in Codex auth file"))?;
    let field = |name| {
        auth["tokens"][name]
            .as_str()
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| auth_error("Codex auth file lacks access_token/account_id"))
    };
    Ok((field("access_token")?, field("account_id")?))
}

/// Provider URLs are HTTP(S) without credentials, query or fragment.
pub fn validate_url(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).map_err(|_| anyhow!("Invalid provider URL"))?;
    anyhow::ensure!(
        matches!(parsed.scheme(), "http" | "https")
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none()
            && parsed.fragment().is_none(),
        "Provider URL must be HTTP(S) without credentials/query/fragment"
    );
    Ok(())
}
