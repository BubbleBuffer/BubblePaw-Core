//! HTTP send/status handling for MessagesProvider.
use super::*;

impl MessagesProvider {
    /// Sends the request and checks the status. A success body is returned
    /// unread so the caller can stream it or read it whole.
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
            .post(format!("{}/messages", self.base))
            .header("x-api-key", &self.key)
            .header("anthropic-version", "2023-06-01")
            .header("User-Agent", "bubblepaw/lean-rust");
        if let Some(route_key) = route_key {
            builder = builder.header("x-opencode-session", route_key);
        }
        let dump = crate::adapter::http_dump::HttpDump::begin(body, "messages");
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
                self.client
                    .transport_failed("Messages transport failed".into())
            })?;
        if let Some(dump) = dump {
            dump.attach(&mut response);
        }
        if response.status().is_success() {
            return Ok(response);
        }
        let code = response.status().as_u16();
        let quota = if self.opencode_route {
            Vec::new()
        } else {
            crate::adapter::quota_headers::parse_anthropic_headers(
                "anthropic-messages",
                &crate::current_account().unwrap_or_else(|| "anthropic-messages".to_owned()),
                response.headers(),
                chrono::Utc::now(),
            )
        };
        let retry_after = crate::adapter::http_error::retry_after(response.headers());
        let bytes = crate::adapter::http_error::read_body(response, cancellation).await?;
        if let Some(reason) = crate::adapter::http_error::terminal_reason(&bytes) {
            return Err(with_error_quota(
                crate::adapter::terminal_quota_failure(
                    reason,
                    crate::adapter::http_error::diagnostic("Messages", code, &bytes, &[&self.key]),
                ),
                quota,
            ));
        }
        let kind = crate::adapter::http_error::status_kind(
            code,
            retry_after,
            messages_context_overflow(&bytes),
        );
        Err(with_error_quota(
            crate::adapter::attach_retry_after(
                classify(
                    kind,
                    anyhow!(crate::adapter::http_error::diagnostic(
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
