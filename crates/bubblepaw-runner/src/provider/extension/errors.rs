//! Typed errors from an extension's JSON-RPC replies, and the redacted
//! diagnostic lines kept from its error text and stderr. Never repeat a
//! credential: see `diagnostic_line`.
use super::*;

const MAX_DIAGNOSTIC_BYTES: usize = 300;

/// Values of the instance's credential-named `env` entries, redacted verbatim
/// from anything the extension reports.
pub(super) fn env_secrets(env: &BTreeMap<String, String>) -> Vec<String> {
    env.iter()
        .filter(|(name, value)| {
            let name = name.to_ascii_lowercase();
            value.len() >= 8
                && [
                    "key",
                    "token",
                    "secret",
                    "password",
                    "auth",
                    "credential",
                    "cookie",
                ]
                .iter()
                .any(|part| name.contains(part))
        })
        .map(|(_, value)| value.clone())
        .collect()
}

/// The first nonempty line of `text` with keys, bearer tokens, `NAME=value`
/// assignments, long opaque strings and `secrets` replaced by `[redacted]`,
/// whitespace collapsed and cut to 300 bytes after redaction.
pub(super) fn diagnostic_line(text: &str, secrets: &[String]) -> String {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty());
    let secrets: Vec<&str> = secrets.iter().map(String::as_str).collect();
    super::super::http_error::redacted_line(line.unwrap_or(""), &secrets, MAX_DIAGNOSTIC_BYTES)
}

/// A JSON-RPC error whose `data` carries the provider error kind. A rate limit
/// with a `limit_type` (a subscription usage cap, e.g. `five_hour`) keeps that
/// type and `resets_at` in the message, so it is distinguishable from a plain 429.
pub(super) fn rpc_error(error: &Value, secrets: &[String]) -> ProviderError {
    let data = error.get("data");
    let kind = data
        .and_then(|data| serde_json::from_value(data.clone()).ok())
        .unwrap_or(ProviderErrorKind::Fatal);
    let raw_message = error.get("message").and_then(Value::as_str).unwrap_or("");
    let message = if raw_message == "cancelled" {
        "cancelled".to_owned()
    } else if raw_message.starts_with("output_schema") {
        "extension rejected output_schema".to_owned()
    } else if raw_message.starts_with("reasoning_effort") {
        "extension rejected reasoning_effort".to_owned()
    } else {
        match &kind {
            ProviderErrorKind::Auth => "extension authentication failed".to_owned(),
            ProviderErrorKind::RateLimit { resets_at } => {
                let limit_type = data
                    .and_then(|data| data.get("limit_type"))
                    .and_then(Value::as_str)
                    .filter(|limit| safe_token(limit));
                match limit_type {
                    Some(limit) => {
                        let mut message = format!("extension usage limit reached: {limit}");
                        if let Some(resets_at) = resets_at.as_deref().filter(|at| safe_token(at)) {
                            message.push_str(&format!(", resets {resets_at}"));
                        }
                        message
                    }
                    None => "extension rate limit reached".to_owned(),
                }
            }
            ProviderErrorKind::Context => "extension context limit exceeded".to_owned(),
            ProviderErrorKind::Transient => "extension request failed transiently".to_owned(),
            // The one kind whose fixed message says nothing about what broke:
            // keep a redacted, truncated line of the extension's own text.
            ProviderErrorKind::Fatal => match diagnostic_line(raw_message, secrets) {
                detail if detail.is_empty() => "extension request failed".to_owned(),
                detail => format!("extension request failed: {detail}"),
            },
        }
    };
    let message = if data
        .and_then(|data| data.get("terminal_quota"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        "billing or spend limit reached: extension rejected account billing or credits".to_owned()
    } else {
        message
    };
    let kind = if message.starts_with("billing or spend limit reached") {
        ProviderErrorKind::Fatal
    } else {
        kind
    };
    let reported = data
        .and_then(|data| data.get("usage"))
        .and_then(|usage| serde_json::from_value::<Usage>(usage.clone()).ok());
    let error = ProviderError::new(kind, message).with_quota(extension_quota(
        data,
        "claude-code",
        "claude-code",
    ));
    match reported {
        Some(usage) => error.with_usage(usage),
        None => error,
    }
}

/// The full model ID an extension reported for a step or error `data`. Anything
/// that is not a short identifier is ignored, so the configured name is used.
pub(super) fn reported_model_of(value: Option<&Value>) -> Option<String> {
    value?
        .get("reported_model")?
        .as_str()
        .filter(|model| safe_model(model))
        .map(str::to_owned)
}

fn safe_model(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | ':'))
}

pub(super) fn extension_quota(
    value: Option<&Value>,
    provider: &str,
    account: &str,
) -> Vec<bubblepaw_core::QuotaReading> {
    value
        .and_then(|value| value.get("quota_events"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|event| {
            crate::quota_sources::parse_claude_rate_limit(
                provider,
                account,
                event,
                chrono::Utc::now(),
            )
        })
        .collect()
}

/// A short identifier or timestamp that is safe to repeat in an error message.
fn safe_token(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':' | '.' | '+'))
}
