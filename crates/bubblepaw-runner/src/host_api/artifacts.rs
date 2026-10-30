use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
#[derive(Serialize, Deserialize)]
pub struct ArtifactData {
    pub artifact: crate::artifacts::ArtifactRef,
    pub data: String,
}
pub fn list(
    ctx: &HostCtx,
    input: SessionParams,
) -> Result<Vec<crate::artifacts::ArtifactRef>, HostError> {
    ctx.ensure_read_session(&input.session_id)?;
    let artifacts = ctx
        .artifacts
        .as_deref()
        .ok_or_else(|| HostError::new(404, "Artifacts unavailable"))?;
    artifacts.list(&input.session_id).map_err(Into::into)
}
pub fn read(ctx: &HostCtx, input: ReadFileParams) -> Result<ArtifactData, HostError> {
    ctx.ensure_read_session(&input.session_id)?;
    let artifacts = ctx
        .artifacts
        .as_deref()
        .ok_or_else(|| HostError::new(404, "Artifacts unavailable"))?;
    let (artifact, bytes) = artifacts
        .read(&input.session_id, &input.id)
        .map_err(|e| HostError::new(404, e.to_string()))?;
    Ok(ArtifactData {
        artifact,
        data: STANDARD.encode(bytes),
    })
}
pub(super) fn command(ctx: &HostCtx, method: MethodId, params: Value) -> Result<Value, HostError> {
    if method == MethodId::ArtifactsList {
        encode(list(ctx, decode(params)?)?)
    } else {
        encode(read(ctx, decode(params)?)?)
    }
}
