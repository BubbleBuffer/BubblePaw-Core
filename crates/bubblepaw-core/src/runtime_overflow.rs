//! One shrink-and-retry when the provider rejects a main request as too long.
//! Token estimates are approximate, so the host's own check can pass a request
//! the provider then refuses. The host is asked to prepare again under
//! pressure, which compacts regardless of its usual threshold.
use crate::{ModelRequest, PrepareMode, ProviderError, ProviderErrorKind, prepared_request_bytes};
use anyhow::Error;

#[derive(Default)]
pub(super) struct OverflowRetry {
    /// The request that failed, as `(size in bytes, the provider's error)`.
    pending: Option<(usize, Error)>,
    sent_bytes: usize,
    used: bool,
}

impl OverflowRetry {
    /// Record the size of the request about to be sent.
    pub(super) fn sent(&mut self, request: &ModelRequest) {
        self.sent_bytes = prepared_request_bytes(request).unwrap_or(0);
    }

    /// Arm the retry for a rejected request, or hand the error back when it is
    /// not an overflow, output was already published, or the retry was spent.
    pub(super) fn arm(&mut self, error: Error) -> Result<(), Error> {
        let overflow = ProviderError::of(&error)
            .is_some_and(|e| e.kind == ProviderErrorKind::Context && !e.published);
        if self.used || !overflow {
            return Err(error);
        }
        self.used = true;
        self.pending = Some((self.sent_bytes, error));
        Ok(())
    }

    /// The preparation mode for the next attempt: the failed request's size,
    /// at about four bytes a token, is reserved by half to force a real cut.
    pub(super) fn mode(&self) -> PrepareMode<'static> {
        match &self.pending {
            Some((bytes, _)) => PrepareMode::Pressure {
                additional_tokens: bytes / 8,
                additional_bytes: 0,
            },
            None => PrepareMode::Request,
        }
    }

    /// The re-prepared request must be smaller than the one the provider refused.
    pub(super) fn check_smaller(&mut self, request: &ModelRequest) -> Result<(), Error> {
        let Some((failed, error)) = self.pending.take() else {
            return Ok(());
        };
        if prepared_request_bytes(request).map_or(true, |bytes| bytes >= failed) {
            return Err(error);
        }
        Ok(())
    }

    /// A preparation failure after a rejection keeps the provider's error and
    /// says the compaction failed too.
    pub(super) fn failed(&mut self, preparation: Error) -> Error {
        match self.pending.take() {
            Some((_, error)) => error.context(format!(
                "compaction after the overflow failed: {preparation:#}"
            )),
            None => preparation,
        }
    }

    /// The request was accepted: the next request gets its own retry.
    pub(super) fn reset(&mut self) {
        self.used = false;
    }
}
