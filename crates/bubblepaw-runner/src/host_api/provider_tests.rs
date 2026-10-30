use super::*;
use anyhow::Result;
async fn full(ctx: &HostCtx, method: &str, params: Value) -> Result<Value, HostError> {
    dispatch(ctx, Profile::Full, method, params).await
}

#[tokio::test]
async fn provider_commands_save_probe_apply_reload_remove_and_refresh_config() -> Result<()> {
    let root = tempfile::tempdir()?;
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        "[model]\nprovider='demo'\nmain='demo/first'\n[providers.demo]\nkind='scripted'\nmodels=[{id='first'},{id='second'}]\n",
    )?;
    let options = crate::config::LoadOptions {
        workspace: root.path().into(),
        home: None,
        custom_file: Some(config.clone()),
        env: Vec::new(),
        cli: Vec::new(),
    };
    let effective = crate::config::EffectiveConfig::load(options.clone())?;
    let live = Arc::new(crate::provider::LiveProviderConfig::new(effective.clone())?);
    let setup =
        crate::provider_setup::ProviderSetup::new(options, &effective)?.with_live(live.clone());
    let mut ctx = HostCtx::for_store(Store::in_memory()?, root.path());
    ctx.providers = Some(setup);
    ctx.config =
        json!({"workspace":root.path(),"model":"demo/first","provider":"demo","read_only":false});
    let initial = full(&ctx, "providers.list", json!({})).await?;
    assert_eq!(initial["active_main"], "demo/first");
    let input = json!({"kind":"scripted","models":[{"id":"new"}]});
    full(
        &ctx,
        "providers.save",
        json!({"name":"scratch","input":input}),
    )
    .await?;
    assert_eq!(
        full(&ctx, "providers.test", json!({"name":"scratch"})).await?["config"]["ok"],
        true
    );
    assert_eq!(
        full(
            &ctx,
            "providers.test",
            json!({"name":"draft","input":input})
        )
        .await?["auth"],
        "not_applicable"
    );
    full(&ctx, "roles.set_main", json!({"model":"scratch/new"})).await?;
    assert_eq!(
        full(&ctx, "config.get", json!({})).await?["model"],
        "scratch/new"
    );
    assert_eq!(
        full(&ctx, "config.get", json!({})).await?["provider"],
        "scratch"
    );
    assert_eq!(
        full(&ctx, "providers.remove", json!({"name":"scratch"}))
            .await
            .unwrap_err()
            .http_status(),
        409
    );
    full(&ctx, "roles.set_main", json!({"model":"demo/second"})).await?;
    let reloaded = full(&ctx, "providers.reload", json!({})).await?;
    assert_eq!(reloaded["active_main"], "demo/second");
    full(&ctx, "providers.remove", json!({"name":"scratch"})).await?;
    assert_eq!(
        full(&ctx, "providers.test", json!({"name":"scratch"}))
            .await
            .unwrap_err()
            .http_status(),
        404
    );
    // Config reads report the accepted runtime, even when an external edit
    // cannot be loaded. A host need not expose provider editing to report it.
    std::fs::write(&config, "[invalid external edit")?;
    assert_eq!(
        full(&ctx, "config.get", json!({})).await?["model"],
        "demo/second"
    );
    ctx.provider_config = Some(live);
    ctx.providers = None;
    std::fs::remove_file(&config)?;
    let active = full(&ctx, "config.get", json!({})).await?;
    assert_eq!(active["model"], "demo/second");
    assert_eq!(active["provider"], "demo");
    Ok(())
}
