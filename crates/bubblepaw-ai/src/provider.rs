use crate::{
    Branch, CancellationToken, Capabilities, ModelInfo, ModelRequest, ModelResponse,
    StreamCallback, WireShape,
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

#[async_trait]
pub trait Provider: Send + Sync {
    /// The isolated branch ability of this exact route, or None when the
    /// model cannot append a branch while preserving the main request's
    /// native prefix and generation settings.
    fn branch(&self, _model: &str) -> Option<&dyn Branch> {
        None
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
