//! Bounded HTTP rejection diagnostics and the one HTTP status mapping used by
//! every provider adapter. Never include request headers or credentials.
use super::{cancelled, stream};
use anyhow::Result;
use bubblepaw_core::{CancellationToken, ProviderErrorKind};
use regex::Regex;
use serde_json::Value;
use std::{sync::LazyLock, time::Duration};

const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_DIAGNOSTIC_BYTES: usize = 4096;
const REDACTED: &str = "[redacted]";

/// The server-requested wait. This is the only provider code that reads the
/// header; a past HTTP date means no delay.
pub(super) fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(super::parse_retry_after)
}

/// RFC 3339 instant a server wait ends, for a typed rate-limit kind.
pub(super) fn resets_at(delay: Option<Duration>) -> Option<String> {
    delay
        .and_then(|delay| chrono::Duration::from_std(delay).ok())
        .and_then(|delay| chrono::Utc::now().checked_add_signed(delay))
        .map(|time| time.to_rfc3339())
}

/// The classified kind for an HTTP rejection. Adapters keep their own
/// context-overflow detection and pass its result.
pub(super) fn status_kind(
    code: u16,
    retry_after: Option<Duration>,
    context_overflow: bool,
) -> ProviderErrorKind {
    match code {
        401 | 403 => ProviderErrorKind::Auth,
        429 => ProviderErrorKind::RateLimit {
            resets_at: resets_at(retry_after),
        },
        400 if context_overflow => ProviderErrorKind::Context,
        408 | 409 | 500..=599 => ProviderErrorKind::Transient,
        _ => ProviderErrorKind::Fatal,
    }
}

/// Billing and enforced spend controls are terminal even when a gateway uses
/// HTTP 429. Retrying cannot replenish credits or alter the account's budget.
pub(super) fn terminal_billing(body: &[u8]) -> bool {
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    [
        "enforced_spend_limit_reached",
        "billing_error",
        "insufficient_quota",
        "insufficient_credits",
        "credit_balance_too_low",
        "credit balance is too low",
        "out of credits",
        "credits_required",
        "spend limit reached",
        "spending limit reached",
        "billing limit",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}
pub(super) fn terminal_reason(body: &[u8]) -> Option<&'static str> {
    if terminal_billing(body) {
        return Some("billing or spend limit reached");
    }
    let payload: Value = serde_json::from_slice(body).ok()?;
    [&payload, &payload["error"], &payload["response"]["error"]]
        .into_iter()
        .any(|error| {
            error.get("retryable").and_then(Value::as_bool) == Some(false)
                || error.get("non_retryable").and_then(Value::as_bool) == Some(true)
        })
        .then_some("provider marked request non-retryable")
}

/// Keep error responses bounded even when an endpoint sends an endless body.
/// Failed or timed-out reads retain the prefix received; cancellation still wins.
pub(super) async fn read_body(
    mut response: reqwest::Response,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>> {
    if cancellation.is_cancelled() {
        return Err(cancelled());
    }
    let read = async {
        let mut body = Vec::new();
        let deadline = tokio::time::Instant::now() + stream::IDLE_TIMEOUT;
        while body.len() < MAX_BODY_BYTES {
            match tokio::time::timeout_at(deadline, response.chunk()).await {
                Ok(Ok(Some(chunk))) => {
                    super::http_dump::append(&response, &chunk);
                    let remaining = MAX_BODY_BYTES - body.len();
                    body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                }
                Ok(Ok(None) | Err(_)) | Err(_) => break,
            }
        }
        body
    };
    tokio::select! {
        body = read => Ok(body),
        _ = cancellation.cancelled() => Err(cancelled()),
    }
}

pub(super) fn diagnostic(provider: &str, code: u16, body: &[u8], secrets: &[&str]) -> String {
    let mut output = format!("{provider} HTTP {code}");
    if body.is_empty() {
        return output;
    }
    let raw = String::from_utf8_lossy(body);
    let detail = if let Ok(mut value) = serde_json::from_str::<Value>(&raw) {
        redact_fields(&mut value);
        let error = value.get("error").unwrap_or(&value);
        let mut fields = Vec::new();
        for field in ["type", "code", "message"] {
            if let Some(text) = error.get(field).and_then(Value::as_str)
                && !text.is_empty()
            {
                fields.push(format!("{field}={text}"));
            }
        }
        if fields.is_empty() {
            value.to_string()
        } else {
            format!("{}; body={value}", fields.join("; "))
        }
    } else {
        raw.into_owned()
    };
    let detail = redact_text(&detail, secrets);
    let detail = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    if !detail.is_empty() {
        output.push_str(": ");
        output.push_str(&detail);
    }
    if body.len() >= MAX_BODY_BYTES {
        output.push_str(" [body truncated at read limit]");
    }
    truncate(&mut output, MAX_DIAGNOSTIC_BYTES);
    output
}

fn redact_fields(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                let key = key.to_ascii_lowercase().replace(['-', '_'], "");
                if matches!(
                    key.as_str(),
                    "authorization"
                        | "proxyauthorization"
                        | "apikey"
                        | "xapikey"
                        | "token"
                        | "accesstoken"
                        | "refreshtoken"
                        | "idtoken"
                        | "secret"
                        | "clientsecret"
                        | "password"
                        | "cookie"
                        | "setcookie"
                ) {
                    *value = Value::String(REDACTED.into());
                } else {
                    redact_fields(value);
                }
            }
        }
        Value::Array(array) => array.iter_mut().for_each(redact_fields),
        _ => {}
    }
}

fn redact_text(text: &str, secrets: &[&str]) -> String {
    static HEADERS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?im)^(authorization|proxy-authorization|cookie|set-cookie)\s*:\s*[^\r\n]*")
            .expect("valid header redaction regex")
    });
    static AUTH: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b(bearer|basic)\s+[A-Za-z0-9_./+=:-]+")
            .expect("valid auth redaction regex")
    });
    static FIELD: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)\b(authorization|proxy-authorization|x-api-key|api[-_]?key|token|access[-_]?token|refresh[-_]?token|id[-_]?token|secret|client[-_]?secret|password|cookie|set-cookie)\b(\s*["']?\s*[:=]\s*["']?)[^"'\s,;}\]]+"#)
            .expect("valid secret field redaction regex")
    });
    let mut text = HEADERS.replace_all(text, "$1: [redacted]").into_owned();
    text = AUTH.replace_all(&text, "$1 [redacted]").into_owned();
    text = FIELD.replace_all(&text, "$1$2[redacted]").into_owned();
    // Replace the exact credential before truncation so a cut cannot expose it.
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        text = text.replace(*secret, REDACTED);
    }
    text
}

fn truncate(text: &mut String, limit: usize) {
    const SUFFIX: &str = " [truncated]";
    if text.len() > limit {
        let mut end = limit - SUFFIX.len();
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str(SUFFIX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn oversized_http_body_stops_at_read_cap() {
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    " ".repeat(MAX_BODY_BYTES + 32),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let response = stream::client().unwrap().get(url).send().await.unwrap();
        let body = read_body(response, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(body.len(), MAX_BODY_BYTES);
        assert!(diagnostic("Messages", 400, &body, &[]).contains("body truncated at read limit"));
        server.abort();
    }

    #[test]
    fn fake_400_preserves_provider_validation_error_and_redacts_credentials() {
        let body = json!({"type":"error","error":{
            "type":"invalid_request_error", "code":"invalid_thinking",
            "message":"thinking requires a signed block; x-api-key: synthetic-key; Authorization: Bearer echoed-token",
            "api_key":"other-secret", "details":{"field":"messages.1.content"}
        }}).to_string();
        let error = diagnostic("Messages", 400, body.as_bytes(), &["synthetic-key"]);
        assert!(error.starts_with("Messages HTTP 400"));
        assert!(error.contains("type=invalid_request_error"));
        assert!(error.contains("code=invalid_thinking"));
        assert!(error.contains("thinking requires a signed block"));
        assert!(error.contains("body="));
        assert!(error.contains("messages.1.content"));
        for secret in ["synthetic-key", "echoed-token", "other-secret"] {
            assert!(!error.contains(secret), "{error}");
        }
    }

    #[test]
    fn fallback_body_redacts_nested_secret_fields_and_auth_text() {
        let body = br#"{"details":{"api_key":"hidden","Authorization":"Bearer auth-secret"},"body":"rejected api_key=inline-secret and Bearer message-secret"}"#;
        let error = diagnostic("Responses", 400, body, &[]);
        assert!(error.contains("rejected"));
        for secret in ["hidden", "auth-secret", "inline-secret", "message-secret"] {
            assert!(!error.contains(secret), "{error}");
        }
    }

    #[test]
    fn raw_body_is_bounded_and_utf8_safe() {
        let body = format!(
            "Authorization: Bearer raw-secret\nCookie: session=cookie-secret; csrf=csrf-secret\n{}",
            "é".repeat(5000)
        );
        let error = diagnostic("Chat completions", 400, body.as_bytes(), &[]);
        for secret in ["raw-secret", "cookie-secret", "csrf-secret"] {
            assert!(!error.contains(secret));
        }
        assert!(error.len() <= MAX_DIAGNOSTIC_BYTES);
        assert!(error.ends_with("[truncated]"));
        assert_eq!(diagnostic("Messages", 503, &[], &[]), "Messages HTTP 503");
    }
}
