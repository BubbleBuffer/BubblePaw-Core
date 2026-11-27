//! What the router writes into replies and errors: the served route, the usage of
//! failed attempts, and the reason a route was skipped.
use crate::{
    ModelResponse, ProviderError, ProviderErrorKind, QuotaReading, ServedRoute, Usage,
    adapter::add_usage,
};

/// Usage and quota readings from failed attempts that the retry policy repeated.
#[derive(Default)]
pub struct FailedAttempts {
    pub usage: Option<Usage>,
    pub quota: Vec<QuotaReading>,
}
impl FailedAttempts {
    pub fn record(&mut self, error: &anyhow::Error) {
        if let Some(usage) = ProviderError::of(error).and_then(|error| error.usage.as_ref()) {
            add_usage(&mut self.usage, usage);
        }
        if let Some(typed) = ProviderError::of(error) {
            self.quota.extend(typed.quota.iter().cloned());
        }
    }
}

/// Name the route that served a reply, and the routes that failed before it, as
/// typed fields. The replay state keeps the same keys for readers of older replies.
pub(crate) fn stamp_route(
    response: &mut ModelResponse,
    selected: &str,
    failed: &[String],
    reasons: &[String],
) {
    // A nested router may have stamped the reply already. With nothing failed here,
    // its fallbacks stay, as its keys do in the replay state.
    let inner = response.route.take().filter(|_| failed.is_empty());
    response.route = Some(ServedRoute {
        selected: selected.to_owned(),
        fallback_from: inner
            .as_ref()
            .map_or(failed.to_vec(), |r| r.fallback_from.clone()),
        fallback_reasons: inner.map_or(reasons.to_vec(), |r| r.fallback_reasons),
    });
    let state = response
        .provider_state
        .get_or_insert_with(|| serde_json::json!({}));
    if let Some(state) = state.as_object_mut() {
        state.insert(
            "_bubblepaw_selected_model".into(),
            serde_json::json!(selected),
        );
        if !failed.is_empty() {
            state.insert("_bubblepaw_fallback_from".into(), serde_json::json!(failed));
            state.insert(
                "_bubblepaw_fallback_reasons".into(),
                serde_json::json!(reasons),
            );
        }
    }
}

/// Why a route was skipped, from the classified kind only. An extension usage
/// limit keeps its sanitized limit type and reset time (extension.rs rpc_error).
pub(crate) fn fallback_reason(error: &anyhow::Error) -> String {
    let Some(error) = ProviderError::of(error) else {
        return "provider error".into();
    };
    match &error.kind {
        ProviderErrorKind::RateLimit { .. }
            if error.message.starts_with("extension usage limit reached") =>
        {
            error.message.clone()
        }
        ProviderErrorKind::RateLimit {
            resets_at: Some(resets_at),
        } => format!("rate_limit, resets {resets_at}"),
        ProviderErrorKind::RateLimit { resets_at: None } => "rate_limit".into(),
        ProviderErrorKind::Auth => "auth".into(),
        ProviderErrorKind::Context => "context".into(),
        ProviderErrorKind::Transient => "transient".into(),
        ProviderErrorKind::Fatal => "fatal".into(),
    }
}

/// `error` carrying the usage of the failed attempts and whether output was published.
pub fn with_usage(error: anyhow::Error, usage: Option<&Usage>, published: bool) -> anyhow::Error {
    if usage.is_none() && !published {
        return error;
    }
    let mut typed = ProviderError::of(&error)
        .cloned()
        .unwrap_or_else(|| ProviderError::new(ProviderErrorKind::Fatal, error.to_string()));
    typed.published |= published;
    typed.usage = usage.cloned();
    error.context(typed)
}
