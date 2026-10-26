//! HTTP send/status handling for ChatCompletionsProvider.
use super::*;

impl ChatCompletionsProvider {
    /// Sends the request and checks the status. The body is returned unread so
    /// the caller can stream it.
    pub(super) async fn post(
        &self,
        body: &Value,
        session: Option<&str>,
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
        if let Some(session) = session {
            builder = builder.header("x-opencode-session", session);
        }
        let dump = crate::provider::http_dump::HttpDump::begin(body, "chat-completions");
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
                classify(
                    ProviderErrorKind::Transient,
                    anyhow!("Chat completions transport failed"),
                )
            })?;
        if let Some(dump) = dump {
            dump.attach(&mut response);
        }
        if response.status().is_success() {
            return Ok(response);
        }
        let code = response.status().as_u16();
        let retry_after = crate::provider::http_error::retry_after(response.headers());
        let bytes = crate::provider::http_error::read_body(response, cancellation).await?;
        let terminal = crate::provider::http_error::terminal_reason(&bytes);
        let kind = if terminal.is_some() {
            ProviderErrorKind::Fatal
        } else {
            crate::provider::http_error::compatible_status_kind(
                code,
                retry_after,
                crate::provider::is_context_overflow(&String::from_utf8_lossy(&bytes)),
                &bytes,
            )
        };
        let diagnostic =
            crate::provider::http_error::diagnostic("Chat completions", code, &bytes, &[&self.key]);
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
        let error = crate::provider::attach_retry_after(error.into(), code, retry_after);
        Err(crate::provider::http_error::mark_bare_bad_request(
            error, code, &bytes,
        ))
    }
}
