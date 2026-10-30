use super::*;
#[derive(Serialize, Deserialize)]
pub struct ForkParams {
    pub session_id: String,
    pub title: Option<String>,
}
pub type ForkResult = bubblepaw_core::Session;
pub fn fork_session(ctx: &HostCtx, input: ForkParams) -> Result<ForkResult, HostError> {
    ctx.ensure_session(&input.session_id)?;
    let title = crate::sessions::title(input.title.as_deref(), "New session")
        .map_err(|e| HostError::invalid(e.to_string()))?;
    let session = ctx.store.fork(&input.session_id, title)?;
    ctx.scope.adopt_idle_root(&session.id);
    Ok(session)
}
