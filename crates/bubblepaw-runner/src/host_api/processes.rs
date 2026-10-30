use super::*;
#[derive(Serialize, Deserialize)]
pub struct BackgroundParams {
    pub session_id: String,
    pub handle: String,
}
#[derive(Serialize, Deserialize)]
pub struct ListResult {
    pub jobs: Vec<Value>,
}
pub type BackgroundResult = Value;
pub fn list(ctx: &HostCtx, input: SessionParams) -> Result<ListResult, HostError> {
    ctx.ensure_read_session(&input.session_id)?;
    decode(
        ctx.processes
            .as_ref()
            .map_or_else(|| Ok(json!({"jobs":[]})), |p| p.jobs(&input.session_id))?,
    )
}
pub fn background(ctx: &HostCtx, input: BackgroundParams) -> Result<BackgroundResult, HostError> {
    ctx.ensure_session(&input.session_id)?;
    let processes = ctx
        .processes
        .as_deref()
        .ok_or_else(|| HostError::internal("Processes are unavailable"))?;
    processes
        .background(&input.session_id, &input.handle)
        .map_err(Into::into)
}
pub(super) fn command(ctx: &HostCtx, method: MethodId, params: Value) -> Result<Value, HostError> {
    if method == MethodId::ProcessList {
        encode(list(ctx, decode(params)?)?)
    } else {
        background(ctx, decode(params)?)
    }
}
