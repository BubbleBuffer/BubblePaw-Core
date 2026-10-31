use super::*;
pub(super) async fn command(
    ctx: &HostCtx,
    method: MethodId,
    params: Value,
) -> Result<Value, HostError> {
    match method {
        MethodId::Initialize
        | MethodId::ConfigGet
        | MethodId::ConfigSet
        | MethodId::ConfigReload => super::config::command(ctx, method, params).await,
        MethodId::ProvidersList
        | MethodId::ProvidersSave
        | MethodId::ProvidersRemove
        | MethodId::ProvidersTest
        | MethodId::ProvidersReload
        | MethodId::RolesSetMain => super::providers::command(ctx, method, params).await,
        _ => {
            let ctx = ctx.clone();
            tokio::task::spawn_blocking(move || match method {
                MethodId::EnvironmentInspect | MethodId::EnvironmentControl => {
                    super::environments::command(&ctx, method, params)
                }
                MethodId::ProcessList | MethodId::ProcessBackground => {
                    super::processes::command(&ctx, method, params)
                }
                MethodId::ApprovalList | MethodId::ApprovalDecide => {
                    super::approvals::command(&ctx, method, params)
                }
                MethodId::AttachmentsAdd | MethodId::AttachmentsRead => {
                    super::attachments::command(&ctx, method, params)
                }
                MethodId::ArtifactsList | MethodId::ArtifactsRead => {
                    super::artifacts::command(&ctx, method, params)
                }
                MethodId::StorageStats => encode(ctx.store.storage_stats()?),
                MethodId::Shutdown => Ok(json!({"stopped":true})),
                _ => unreachable!("service method table"),
            })
            .await
            .map_err(|e| HostError::internal(e.to_string()))?
        }
    }
}
