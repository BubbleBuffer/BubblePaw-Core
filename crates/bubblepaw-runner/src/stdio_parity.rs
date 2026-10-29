//! Shared-service setup and dispatch for the additive stdio host controls.
use super::*;

pub(super) fn providers(
    effective: &EffectiveConfig,
    model_provider: &Arc<LiveProviderConfig>,
    live_config: &Arc<LiveConfig>,
) -> Result<ProviderSetup> {
    Ok(ProviderSetup::new(effective.load_options(), effective)?
        .with_live(model_provider.clone())
        .with_host_config(live_config.clone()))
}

pub(super) async fn command(
    store: &Store,
    services: &StdioServices,
    method: &str,
    params: &Value,
) -> Result<Value> {
    Ok(match method {
        "providers.list" => services.providers.list()?,
        "roles.set_main" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct MainModel {
                model: String,
            }
            let input: MainModel = serde_json::from_value(params.clone())?;
            services.providers.set_main(&input.model).await?
        }
        "session.list" => stdio_sessions::list(store, &services.usage, params)?,
        "child.cancel" => {
            let parent = params["parent"].as_str().context("parent is required")?;
            let child = params["child"].as_str().context("child is required")?;
            services.control.ensure_workspace(parent)?;
            services.control.ensure_workspace(child)?;
            services.agents.cancel(parent, child).await?
        }
        _ => bail!("Unknown stdio host control: {method}"),
    })
}
