use super::*;
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetMainParams {
    pub model: String,
}
#[derive(Deserialize, Serialize)]
pub struct ProviderParams {
    pub name: String,
}
#[derive(Deserialize, Serialize)]
pub struct SaveParams {
    pub name: String,
    pub input: Value,
}
#[derive(Deserialize, Serialize)]
pub struct TestParams {
    pub name: String,
    #[serde(default, alias = "draft")]
    pub input: Option<Value>,
}
/// Provider payloads retain provider-specific registry fields and redacted setup metadata.
pub type ProviderResult = Value;
fn service(ctx: &HostCtx) -> Result<&crate::provider_setup::ProviderSetup, HostError> {
    ctx.providers
        .as_ref()
        .ok_or_else(|| HostError::unavailable("Providers not available in this host"))
}
pub fn list(ctx: &HostCtx, _input: EmptyParams) -> Result<ProviderResult, HostError> {
    service(ctx)?.list().map_err(Into::into)
}
pub async fn set_main(ctx: &HostCtx, input: SetMainParams) -> Result<ProviderResult, HostError> {
    service(ctx)?
        .set_main(&input.model)
        .await
        .map_err(Into::into)
}
pub async fn save(ctx: &HostCtx, input: SaveParams) -> Result<ProviderResult, HostError> {
    service(ctx)?
        .save(&input.name, input.input)
        .await
        .map_err(Into::into)
}
pub async fn remove(ctx: &HostCtx, input: ProviderParams) -> Result<ProviderResult, HostError> {
    service(ctx)?.remove(&input.name).await.map_err(Into::into)
}
pub async fn test(ctx: &HostCtx, input: TestParams) -> Result<ProviderResult, HostError> {
    match input.input {
        Some(draft) => service(ctx)?
            .test_draft(&input.name, draft)
            .await
            .map_err(Into::into),
        None => service(ctx)?.test(&input.name).await.map_err(Into::into),
    }
}
pub async fn reload(ctx: &HostCtx, _input: EmptyParams) -> Result<ProviderResult, HostError> {
    service(ctx)?.reload().await.map_err(Into::into)
}
pub(super) async fn command(
    ctx: &HostCtx,
    method: MethodId,
    params: Value,
) -> Result<Value, HostError> {
    match method {
        // Protocol 0.1 ignored params entirely for this no-argument method.
        MethodId::ProvidersList => list(ctx, EmptyParams {}),
        MethodId::RolesSetMain => set_main(ctx, decode(params)?).await,
        MethodId::ProvidersSave => save(ctx, decode(params)?).await,
        MethodId::ProvidersRemove => remove(ctx, decode(params)?).await,
        MethodId::ProvidersTest => test(ctx, decode(params)?).await,
        MethodId::ProvidersReload => reload(ctx, decode(params)?).await,
        _ => unreachable!("provider method table"),
    }
}
