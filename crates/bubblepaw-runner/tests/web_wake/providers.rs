//! Scripted parent and child models for the web wake tests.
use super::*;

#[derive(Default)]
pub(super) struct ParentProvider(pub(super) Mutex<Vec<ModelRequest>>);
#[async_trait]
impl Provider for ParentProvider {
    async fn generate(
        &self,
        request: ModelRequest,
        on_text: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        // Rounds count per session, so wake-barrier roots leave the parent's alone.
        let round = {
            let mut requests = self.0.lock().unwrap();
            let session = request.session_id.clone();
            requests.push(request);
            requests
                .iter()
                .filter(|request| request.session_id == session)
                .count()
        };
        let text = format!("Parent response {round}.");
        on_text(&format!("{text}\n\n"));
        Ok(ModelResponse {
            text,
            ..Default::default()
        })
    }
}

pub(super) struct ChildProvider(pub(super) Semaphore);
#[async_trait]
impl Provider for ChildProvider {
    async fn generate(
        &self,
        _: ModelRequest,
        on_text: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.0.acquire().await?.forget();
        let text = "Synthetic child result.".to_owned();
        on_text(&text);
        Ok(ModelResponse {
            text,
            ..Default::default()
        })
    }
}
