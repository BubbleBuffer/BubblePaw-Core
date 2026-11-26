//! Error and retry-after helpers shared by the adapters.
use crate::{ModelRequest, ProviderError, ProviderErrorKind, QuotaReading, Usage};
use std::time::{Duration, Instant};

/// Remaining server-requested delay, preserved through the provider error chain.
/// Adapters record it; the registry's retry policy decides what to do with it.
pub fn provider_retry_after(error: &anyhow::Error) -> Option<Duration> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<RetryAfterError>())
        .map(|error| error.deadline.saturating_duration_since(Instant::now()))
}

pub fn current_rejection_quota_count(error: &anyhow::Error) -> Option<usize> {
    error
        .downcast_ref::<RejectionQuotaBoundary>()
        .map(|boundary| boundary.count)
}

#[derive(Debug)]
pub(super) struct RejectionQuotaBoundary {
    pub(super) count: usize,
    pub(super) message: String,
}
impl std::fmt::Display for RejectionQuotaBoundary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[derive(Debug)]
struct RetryAfterError {
    deadline: Instant,
    source: anyhow::Error,
}
impl std::fmt::Display for RetryAfterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.source, formatter)
    }
}
impl std::error::Error for RetryAfterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}
pub fn attach_retry_after(
    error: anyhow::Error,
    code: u16,
    delay: Option<Duration>,
) -> anyhow::Error {
    if retryable_http_status(code)
        && let Some(deadline) = delay.and_then(|delay| Instant::now().checked_add(delay))
    {
        RetryAfterError {
            deadline,
            source: error,
        }
        .into()
    } else {
        error
    }
}
/// Attach a kind, keeping the message exactly as before.
pub fn classify(kind: ProviderErrorKind, error: anyhow::Error) -> anyhow::Error {
    if ProviderError::of(&error).is_some() {
        return error;
    }
    ProviderError::new(kind, error.to_string()).into()
}
pub fn cancelled() -> anyhow::Error {
    ProviderError::new(ProviderErrorKind::Transient, "cancelled").into()
}

/// Hosts must not retry or silently route a terminal account billing failure.
pub fn is_terminal_quota_error(error: &ProviderError) -> bool {
    matches!(error.kind, ProviderErrorKind::Fatal)
        && (error.message.starts_with("billing or spend limit reached")
            || error
                .message
                .starts_with("provider marked request non-retryable"))
}
pub fn terminal_quota_failure(reason: &str, diagnostic: String) -> anyhow::Error {
    ProviderError::new(ProviderErrorKind::Fatal, format!("{reason}: {diagnostic}")).into()
}

/// Statuses a transport may retry; Retry-After attachments share this policy.
pub fn retryable_http_status(code: u16) -> bool {
    matches!(code, 408 | 409 | 429 | 500..=599)
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

/// The facts an adapter reports with an error it raised after the provider had
/// already billed the request: the message and the usage, when it is known.
#[derive(Debug)]
pub struct MeteredModelError {
    pub message: String,
    pub usage: Option<Usage>,
}
impl std::fmt::Display for MeteredModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for MeteredModelError {}

/// Attach the usage of a billed request to `error`, typed as fatal.
pub fn metered_model_error(error: anyhow::Error, usage: Option<Usage>) -> anyhow::Error {
    let mut source = ProviderError::new(ProviderErrorKind::Fatal, error.to_string());
    source.usage.clone_from(&usage);
    let metadata = MeteredModelError {
        message: error.to_string(),
        usage,
    };
    error.context(source).context(metadata)
}

/// The runtime owns submission validation, including runs whose frozen
/// declarations omit this reserved capability or retain an older schema.
pub(super) fn accepts_call(name: &str, declared: bool) -> bool {
    name == "submit_result" || declared
}

pub fn accepts_request_call(request: &ModelRequest, name: &str) -> bool {
    accepts_call(name, request.tools.iter().any(|tool| tool.name == name))
}
