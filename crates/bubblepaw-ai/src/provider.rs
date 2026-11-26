use crate::{
    CancellationToken, Capabilities, ModelInfo, ModelRequest, ModelResponse, StreamCallback,
    WireShape,
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

#[async_trait]
pub trait Provider: Send + Sync {
    /// Ordered, explicitly configured routes for an isolated summary branch.
    /// This only describes candidates: it must not update main routing
    /// or dispatch a request. The captured successful main route comes first.
    fn summary_fork_routes(&self, model: &str) -> Vec<String> {
        vec![model.to_owned()]
    }
    /// Estimate one explicitly selected fork route, without including other
    /// possible main-request fallbacks in its admission calculation.
    fn estimate_summary_fork_tokens(
        &self,
        request: &ModelRequest,
        count: &dyn Fn(&Value) -> usize,
    ) -> Result<Option<usize>> {
        self.estimate_input_tokens(request, count)
    }
    /// Whether this exact route can append an isolated summary branch
    /// while preserving the main request's native prefix and generation settings.
    fn supports_summary_fork(&self, _model: &str) -> bool {
        false
    }
    /// Canonical source IDs mapped to the actual rendered input positions.
    /// A missing projection cannot be silently replaced with bounded summary input.
    fn summary_fork_mapping(&self, _request: &ModelRequest) -> Result<Option<Value>> {
        Ok(None)
    }
    /// Dispatch an isolated branch. Never update a main mirror or route
    /// selection. Dispatch exactly one route; the caller owns explicit retries
    /// across `summary_fork_routes` while retaining the branch input shape.
    async fn generate_summary_fork(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        anyhow::bail!("Provider does not support isolated summary forks")
    }
    fn models(&self) -> Vec<ModelInfo> {
        Vec::new()
    }
    async fn generate(
        &self,
        request: ModelRequest,
        on_text: StreamCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse>;
    /// Generate one JSON decision sample using the request's reasoning effort.
    async fn generate_decision(
        &self,
        request: ModelRequest,
        on_text: StreamCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        self.generate(request, on_text, cancellation).await
    }
    fn capabilities(&self, _model: &str) -> Capabilities {
        Capabilities::default()
    }
    /// Optional pure projection of model input for a host's local token estimate.
    /// Reuse the adapter's renderer; omit credentials and non-replayed archives.
    /// None leaves the host's canonical estimator in place. This is not a
    /// provider tokenizer or a network request.
    fn estimate_input_tokens(
        &self,
        _request: &ModelRequest,
        _count: &dyn Fn(&Value) -> usize,
    ) -> Result<Option<usize>> {
        Ok(None)
    }
    /// Optional pure projection of the exact wire body for prompt-cache
    /// accounting (see `WireShape`). None skips accounting for this request.
    fn wire_shape(&self, _request: &ModelRequest) -> Option<WireShape> {
        None
    }
    /// The conversation was deleted or moved to another provider; drop any mirror.
    async fn close_session(&self, _session_id: &str) -> Result<()> {
        Ok(())
    }
}
