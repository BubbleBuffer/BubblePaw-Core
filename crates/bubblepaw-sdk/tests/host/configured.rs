//! `Client::open_configured`: the CLI host's own context, configured the way
//! `bubblepaw` configures it, embedded in this process or attached to.
use anyhow::Result;
use bubblepaw_sdk::{Client, ConfigError, ConfigOptions, HostConfig};
use serde_json::json;
use std::path::Path;

const CONFIG: &str = "\
[model]
provider='demo'
main='demo/first'
[providers.demo]
kind='scripted'
models=[{id='first'},{id='second'}]
[roles]
summary=['demo/second','main']
[context]
auto_summarize=false
[agents]
auto_wake=false
";

fn options(root: &Path) -> ConfigOptions {
    ConfigOptions {
        workspace: Some(root.to_owned()),
        data_dir: Some(root.join("data")),
        config_file: Some(root.join("config.toml")),
        home: None,
        env: Some(Vec::new()),
        overrides: Vec::new(),
    }
}

#[tokio::test]
async fn open_configured_embeds_the_cli_host_and_attaches_when_owned() -> Result<()> {
    let root = tempfile::tempdir()?;
    std::fs::write(root.path().join("config.toml"), CONFIG)?;
    let config = HostConfig::load(options(root.path()))?;
    assert_eq!(
        config.settings().roles.summary.models,
        ["demo/second", "demo/first"]
    );
    let owner = Client::open_configured(config).await?;
    assert!(
        owner.host().is_none(),
        "a configured host is not a built one"
    );
    assert!(
        root.path().join("data/host.json").exists(),
        "the embedded host publishes its endpoint"
    );
    let config_get = owner.call("config.get", json!({})).await?;
    assert_eq!(config_get["model"], "demo/first");
    let providers = owner.call("providers.list", json!({})).await?;
    assert_eq!(providers["main"], "demo/first");

    let second = Client::open_configured(HostConfig::load(options(root.path()))?).await?;
    assert_eq!(second.call("providers.list", json!({})).await?, providers);
    assert_eq!(second.call("config.get", json!({})).await?, config_get);

    // Roles and live provider reload work on the CLI host context.
    owner
        .call("roles.set_main", json!({"model":"demo/second"}))
        .await?;
    assert_eq!(
        second.call("config.get", json!({})).await?["model"],
        "demo/second"
    );
    // Closing the attached client leaves the owner serving.
    second.close().await?;
    owner.call("session.list", json!({})).await?;
    owner.close().await?;
    assert!(!root.path().join("data/host.json").exists());

    // Once the owner has stopped, the store is free to embed again.
    let reopened = Client::embedded_configured(HostConfig::load(options(root.path()))?).await?;
    reopened.close().await?;
    Ok(())
}

#[test]
fn configuration_errors_are_typed() {
    let root = tempfile::tempdir().unwrap();
    match HostConfig::load(options(root.path())) {
        Err(ConfigError::File { path, .. }) => assert_eq!(path, root.path().join("config.toml")),
        other => panic!("expected a file error, got {:?}", other.err()),
    }
    std::fs::write(
        root.path().join("config.toml"),
        CONFIG.replace("[context]", "decider=['llm/nowhere']\n[context]"),
    )
    .unwrap();
    assert!(matches!(
        HostConfig::load(options(root.path())),
        Err(ConfigError::Roles(_))
    ));
}
