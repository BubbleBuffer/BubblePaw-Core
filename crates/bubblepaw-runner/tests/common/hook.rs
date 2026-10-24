//! The host hook's preparation as the harness calls it, shared by the runner's
//! integration tests. Each test file includes this with `#[path]`.
#![allow(dead_code)]
use anyhow::Result;
use bubblepaw_core::{
    CancellationToken, ContextPreparation, ContextState, ContextWriter, Message, ModelRequest,
    PrepareCall, PrepareMode, PreparedContext, Provider,
};
use bubblepaw_runner::episode_background::EpisodeTemplate;
use std::sync::Arc;

/// One ordinary request preparation on a fresh writer; what the hook wrote is dropped.
pub async fn prepare_fresh(
    context: &dyn ContextPreparation,
    provider: Arc<dyn Provider>,
    session: &str,
    request: ModelRequest,
) -> Result<PreparedContext> {
    let state = ContextState::default();
    prepare_call(
        context,
        provider,
        &state,
        session,
        request,
        PrepareMode::Request,
        CancellationToken::new(),
    )
    .await
}

/// One preparation on a writer over `state`, so the test can read what the hook wrote.
pub async fn prepare_call(
    context: &dyn ContextPreparation,
    provider: Arc<dyn Provider>,
    state: &ContextState,
    session: &str,
    request: ModelRequest,
    mode: PrepareMode<'_>,
    cancellation: CancellationToken,
) -> Result<PreparedContext> {
    let writer = ContextWriter::new(state, session);
    context
        .prepare(PrepareCall {
            session_id: session,
            request,
            provider,
            writer: &writer,
            mode,
            cancellation,
        })
        .await
}

/// The request the scripted tests prepare: fixed instructions over `messages`.
/// A template that always returns the same request, as a host with a fixed prompt would.
pub fn fixed_template(request: ModelRequest) -> EpisodeTemplate {
    Arc::new(move |_: &str| -> anyhow::Result<ModelRequest> { Ok(request.clone()) })
}

pub fn scripted_request(messages: Vec<Message>) -> ModelRequest {
    ModelRequest {
        model: "script".into(),
        system_prompt: "instructions".into(),
        messages,
        ..Default::default()
    }
}
