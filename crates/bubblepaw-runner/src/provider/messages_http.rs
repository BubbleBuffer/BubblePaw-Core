//! HTTP send/status handling for MessagesProvider.
use super::*;

impl MessagesProvider {
    /// Sends the request and checks the status. A success body is returned
    /// unread so the caller can stream it or read it whole.
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
            .post(format!("{}/messages", self.base))
            .header("x-api-key", &self.key)
            .header("anthropic-version", "2023-06-01")
            .header("User-Agent", "bubblepaw/lean-rust");
        if let Some(session) = session {
            builder = builder.header("x-opencode-session", session);
        }
        let dump = crate::provider::http_dump::HttpDump::begin(body, "messages");
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
                    anyhow!("Messages response was idle too long"),
                )
            })?
            .map_err(|_| {
                classify(
                    ProviderErrorKind::Transient,
                    anyhow!("Messages transport failed"),
                )
            })?;
        if let Some(dump) = dump {
            dump.attach(&mut response);
        }
        if response.status().is_success() {
            return Ok(response);
        }
        let code = response.status().as_u16();
        let identity = bubblepaw_core::RequestUsage::new(
            "anthropic-messages",
            body["model"].as_str().unwrap_or_default(),
        );
        let quota = if self.opencode_route {
            Vec::new()
        } else {
            crate::quota_sources::parse_anthropic_headers(
                &identity.provider,
                &identity.account,
                response.headers(),
                chrono::Utc::now(),
            )
        };
        let retry_after = crate::provider::http_error::retry_after(response.headers());
        let bytes = crate::provider::http_error::read_body(response, cancellation).await?;
        if let Some(reason) = crate::provider::http_error::terminal_reason(&bytes) {
            return Err(crate::quota_sources::with_error_quota(
                crate::provider::terminal_quota_failure(
                    reason,
                    crate::provider::http_error::diagnostic("Messages", code, &bytes, &[&self.key]),
                ),
                quota,
            ));
        }
        let kind = crate::provider::http_error::status_kind(
            code,
            retry_after,
            messages_context_overflow(&bytes),
        );
        Err(crate::quota_sources::with_error_quota(
            crate::provider::attach_retry_after(
                classify(
                    kind,
                    anyhow!(crate::provider::http_error::diagnostic(
                        "Messages",
                        code,
                        &bytes,
                        &[&self.key]
                    )),
                ),
                code,
                retry_after,
            ),
            quota,
        ))
    }
}
