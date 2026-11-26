use crate::ContextWriter;
pub use crate::prepare_types::{PrepareMode, PreparedContext};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use bubblepaw_ai::{CancellationToken, Message, ModelRequest, ModelResponse, Provider};
use std::sync::Arc;

/// The hook shapes a request view without rewriting canonical history.
#[async_trait]
pub trait ContextPreparation: Send + Sync {
    /// Projection of canonical history into the request's ordinary messages.
    /// Steering batches and reminder previews are appended after it, so a
    /// projection that drops older input never drops them.
    fn project_history(&self, history: &[Message]) -> Result<Vec<Message>> {
        Ok(history.to_vec())
    }

    /// A host may serve a frozen summarized prefix and request only its raw
    /// canonical tail. None retains ordinary history; original rows stay
    /// available through Store's explicit history/range APIs.
    fn history_frontier(&self, _session_id: &str) -> Result<Option<i64>> {
        Ok(None)
    }

    /// The host hook for main requests and manual compaction. Per-request dynamic
    /// context goes through `call.writer` (tail reminders, cold notes), never into
    /// the system prompt: a frozen-tier edit made here is an undeclared
    /// `frozen_change`. Steering or final budget pressure may invoke this again
    /// over fresh canonical history. Auxiliary usage is counted for each invocation,
    /// even when a newer candidate supersedes the prepared request.
    async fn prepare(&self, call: PrepareCall<'_>) -> Result<PreparedContext> {
        match call.mode {
            PrepareMode::Request | PrepareMode::Pressure { .. } => Ok(PreparedContext {
                request: call.request,
                notice: None,
                usage: None,
            }),
            PrepareMode::Compact { .. } => Err(anyhow!("Manual compaction is not configured")),
        }
    }

    /// Observe the final main request after steering, admission and tool/control
    /// shaping, immediately before dispatch. The provider is the same immutable
    /// generation used for this call. Observers must not execute model effects.
    fn observe_request(
        &self,
        _session_id: &str,
        _request: &ModelRequest,
        _provider: Arc<dyn Provider>,
    ) {
    }

    /// Pure final admission after pending reminders have been appended. Hosts
    /// with token/window policy must validate the complete final request here.
    /// Return [`ContextBudgetExceeded`] to retry through [`PrepareMode::Pressure`]
    /// before dispatch; the validator itself must not gather or apply a view.
    fn validate_prepared_request(
        &self,
        _session_id: &str,
        _request: &ModelRequest,
        _provider: Arc<dyn Provider>,
    ) -> Result<()> {
        Ok(())
    }

    /// Positions belonging to the applied compaction view in this exact request.
    /// Diagnostic metadata only; ordinary history has no cold tier.
    fn cold_message_indexes(
        &self,
        _session_id: &str,
        _request: &ModelRequest,
    ) -> Result<Vec<usize>> {
        Ok(Vec::new())
    }

    /// Release disposable context snapshots on session deletion or host stop.
    fn release_session(&self, _session_id: &str) {}

    /// Per-request response usage, before it is combined into run totals.
    fn observe_response(&self, _session_id: &str, _response: &ModelResponse) {}

    /// The reason this context deliberately changed the session's serving
    /// prefix since the last call (for example `lcm_apply`), taken once.
    fn take_prefix_change(&self, _session_id: &str) -> Option<String> {
        None
    }
}

/// A recoverable final admission failure. Display remains the host's original
/// diagnostic; the harness uses the overhead only to re-enter preparation.
#[derive(Debug)]
pub struct ContextBudgetExceeded {
    pub additional_tokens: usize,
    pub additional_bytes: usize,
    pub message: String,
}

impl std::fmt::Display for ContextBudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ContextBudgetExceeded {}

/// One preparation: the session's request, the provider generation captured for
/// it, the root-lineage writer and the cancellation token.
pub struct PrepareCall<'a> {
    pub session_id: &'a str,
    pub request: ModelRequest,
    pub provider: Arc<dyn Provider>,
    pub writer: &'a ContextWriter<'a>,
    pub mode: PrepareMode<'a>,
    pub cancellation: CancellationToken,
}
