//! HTTP send/status handling for ChatCompletionsProvider.
use super::*;

impl ChatCompletionsProvider {
    /// Sends the request and checks the status. The body is returned unread so
    /// the caller can stream it.
    pub(super) async fn post(
        &self,
        body: &Value,
        route_key: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<reqwest::Response> {
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }
        let mut builder = self
            .client
            .post(format!("{}/chat/completions", self.base))
            .header("User-Agent", "bubblepaw/lean-rust");
        if !self.key.is_empty() {
            builder = builder.bearer_auth(&self.key);
        }
        if let Some(route_key) = route_key {
            builder = builder.header("x-opencode-session", route_key);
        }
        let dump = crate::adapter::http_dump::HttpDump::begin(body, "chat-completions");
        let sent = tokio::select! {
            result = tokio::time::timeout(stream::IDLE_TIMEOUT, builder
                .json(body)
                .send()) => result,
            _ = cancellation.cancelled() => return Err(cancelled()),
        };
        let mut response = sent
            .map_err(|_| {
                classify(
                    ProviderErrorKind::Transient,
                    anyhow!("Chat completions response was idle too long"),
                )
            })?
            .map_err(|_| {
                self.client
                    .transport_failed("Chat completions transport failed".into())
            })?;
        if let Some(dump) = dump {
            dump.attach(&mut response);
        }
        if response.status().is_success() {
            return Ok(response);
        }
        let code = response.status().as_u16();
        let retry_after = crate::adapter::http_error::retry_after(response.headers());
        let bytes = crate::adapter::http_error::read_body(response, cancellation).await?;
        let terminal = crate::adapter::http_error::terminal_reason(&bytes);
        let kind = if terminal.is_some() {
            ProviderErrorKind::Fatal
        } else {
            crate::adapter::http_error::compatible_status_kind(
                code,
                retry_after,
                crate::adapter::is_context_overflow(&String::from_utf8_lossy(&bytes)),
                &bytes,
            )
        };
        let diagnostic =
            crate::adapter::http_error::diagnostic("Chat completions", code, &bytes, &[&self.key]);
        let mut error = ProviderError::new(
            kind,
            if let Some(reason) = terminal {
                format!("{reason}: {diagnostic}")
            } else {
                diagnostic
            },
        );
        if let Ok(payload) = serde_json::from_slice::<Value>(&bytes)
            && payload["usage"].is_object()
        {
            error.usage = Some(response_usage(&payload).0);
        }
        let error = crate::adapter::attach_retry_after(error.into(), code, retry_after);
        Err(crate::adapter::http_error::mark_bare_bad_request(
            error, code, &bytes,
        ))
    }
}

/// Error bodies are used for classification only; provider text never reaches
/// the host's error message (it can contain private prompts or credentials).
pub(super) fn chat_error_kind(error: &Value) -> ProviderErrorKind {
    let code = error["status"].as_u64().or_else(|| error["code"].as_u64());
    let label = format!(
        "{} {}",
        error["type"].as_str().unwrap_or_default(),
        error["code"].as_str().unwrap_or_default()
    )
    .to_ascii_lowercase();
    if crate::adapter::is_context_overflow(&error.to_string()) {
        ProviderErrorKind::Context
    } else if matches!(code, Some(401 | 403))
        || label.contains("authentication")
        || label.contains("invalid_api_key")
        || label.contains("permission")
    {
        ProviderErrorKind::Auth
    } else if code == Some(429) || label.contains("rate_limit") {
        ProviderErrorKind::RateLimit { resets_at: None }
    } else if matches!(code, Some(408 | 409 | 500..=599))
        || label.contains("server_error")
        || label.contains("overloaded")
    {
        ProviderErrorKind::Transient
    } else {
        ProviderErrorKind::Fatal
    }
}
