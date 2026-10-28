//! Provider helpers shared by question delivery fixtures.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{CancellationToken, ModelRequest, ModelResponse, Provider, TextCallback};
use std::sync::Mutex;

pub(super) struct NoticeProbe(pub(super) Mutex<Vec<ModelRequest>>);
#[async_trait]
impl Provider for NoticeProbe {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.0.lock().unwrap().push(request);
        Ok(ModelResponse {
            text: "continued".into(),
            ..Default::default()
        })
    }
}
