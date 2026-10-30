use super::*;
use anyhow::Result;

async fn request(ctx: &HostCtx, method: &str, params: Value) -> Value {
    rpc::handle(
        ctx,
        Profile::Full,
        json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}),
    )
    .await
    .expect("request with ID receives a reply")
}

#[tokio::test]
async fn legacy_noarg_open_and_list_accept_null_or_nonobject_rpc_params() -> Result<()> {
    let root = tempfile::tempdir()?;
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        "[model]\nmain='demo/fixture'\nprovider='demo'\n[providers.demo]\nkind='scripted'\nmodels=[{id='fixture'}]\n",
    )?;
    let options = crate::config::LoadOptions {
        workspace: root.path().into(),
        home: None,
        custom_file: Some(config),
        env: Vec::new(),
        cli: Vec::new(),
    };
    let effective = crate::config::EffectiveConfig::load(options.clone())?;
    let mut ctx = HostCtx::for_store(Store::in_memory()?, root.path());
    ctx.providers = Some(crate::provider_setup::ProviderSetup::new(
        options, &effective,
    )?);
    let initialized = request(&ctx, "initialize", json!({})).await;
    let providers = request(&ctx, "providers.list", json!({})).await;
    for params in [Value::Null, json!([]), json!(true), json!("ignored")] {
        assert_eq!(
            request(&ctx, "initialize", params.clone()).await,
            initialized
        );
        assert_eq!(
            request(&ctx, "providers.list", params.clone()).await,
            providers
        );
        let opened = request(&ctx, "session.open", params.clone()).await;
        assert!(opened.get("error").is_none(), "{opened}");
        assert_eq!(opened["result"]["title"], "New session");
        let listed = request(&ctx, "session.list", params).await;
        assert!(listed["result"].is_array(), "{listed}");
        assert!(
            listed["result"]
                .as_array()
                .unwrap()
                .iter()
                .any(|session| session["id"] == opened["result"]["id"])
        );
    }
    // A strict mutation keeps its existing decoder and rejects null.
    assert!(
        request(&ctx, "roles.set_main", Value::Null)
            .await
            .get("error")
            .is_some()
    );
    Ok(())
}
