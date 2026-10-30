use super::*;
#[derive(Serialize, Deserialize)]
pub struct ControlParams {
    pub session_id: String,
    #[serde(default)]
    pub control: Option<Value>,
}
#[derive(Serialize, Deserialize)]
pub struct InspectResult {
    pub environments: Vec<Value>,
}
pub type ControlResult = Value;
pub fn inspect(ctx: &HostCtx, input: SessionParams) -> Result<InspectResult, HostError> {
    ctx.ensure_read_session(&input.session_id)?;
    decode(ctx.repl.as_ref().map_or_else(
        || json!({"environments":[]}),
        |repl| repl.status(&input.session_id),
    ))
}
pub fn control(ctx: &HostCtx, input: ControlParams) -> Result<ControlResult, HostError> {
    ctx.ensure_session(&input.session_id)?;
    let repl = ctx
        .repl
        .as_deref()
        .ok_or_else(|| HostError::internal("REPL is unavailable"))?;
    repl.control(
        &input.session_id,
        input.control.unwrap_or_else(|| json!({"action":"inspect"})),
    )
    .map_err(Into::into)
}
pub(super) fn command(ctx: &HostCtx, method: MethodId, params: Value) -> Result<Value, HostError> {
    if method == MethodId::EnvironmentInspect {
        encode(inspect(ctx, decode(params)?)?)
    } else {
        control(ctx, decode(params)?)
    }
}
