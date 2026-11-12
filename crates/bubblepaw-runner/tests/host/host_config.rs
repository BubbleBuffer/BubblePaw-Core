//! `HostConfig::load` against the CLI: a host context built from it in this
//! process and `bubblepaw stdio` on the same data directory report the same
//! providers and model roles.
use bubblepaw_runner::{
    host_client::HostClient,
    host_config::{ConfigOptions, HostConfig, prepare_store},
    host_lifetime::HostLifetime,
    session_host::host_context::HostContext,
};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};

const CONFIG: &str = "\
[model]
provider='demo'
main='demo/first'
[providers.demo]
kind='scripted'
models=[{id='first'},{id='second'}]
[roles]
summary=['demo/second','main']
chapter=['summary']
[context]
auto_summarize=false
[agents]
auto_wake=false
";

/// The environment both hosts see: this process's, without `BUBBLEPAW_*`.
fn environment() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(name, _)| !name.starts_with("BUBBLEPAW_"))
        .collect()
}

async fn snapshot(client: &HostClient) -> (Value, Value) {
    let providers = client.call("providers.list", json!({})).await.unwrap();
    let config = client.call("config.get", json!({})).await.unwrap();
    (providers, config)
}

/// `bubblepaw` with the configuration both hosts use.
fn cli(root: &Path, subcommand: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_bubblepaw"));
    command
        .arg("--config")
        .arg(root.join("config.toml"))
        .arg("--workspace")
        .arg(root)
        .arg("--data-dir")
        .arg(root.join("data"))
        .arg(subcommand)
        .env_clear()
        .envs(environment());
    command
}

async fn cli_config_file(root: &Path) -> String {
    let output = cli(root, "config").output().await.unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}

async fn cli_host(root: &Path) -> (Value, Value) {
    let mut command = cli(root, "stdio");
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().unwrap();
    let client = HostClient::attach(&root.join("data"), Duration::from_secs(60))
        .await
        .unwrap();
    let seen = snapshot(&client).await;
    client.close().await;
    // Closing stdin ends `bubblepaw stdio` and releases the store.
    drop(child.stdin.take());
    assert!(child.wait().await.unwrap().success());
    seen
}

#[tokio::test]
async fn a_configured_host_sees_the_cli_hosts_providers_and_roles() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    std::fs::write(root.join("config.toml"), CONFIG).unwrap();
    let (cli_providers, cli_config) = cli_host(&root).await;

    let config = HostConfig::load(ConfigOptions {
        workspace: Some(root.clone()),
        data_dir: Some(root.join("data")),
        config_file: Some(root.join("config.toml")),
        env: Some(environment()),
        ..ConfigOptions::default()
    })
    .unwrap();
    let models = |role: &bubblepaw_runner::model_roles::ModelRole| role.models.clone();
    assert_eq!(
        models(&config.settings().roles.summary),
        ["demo/second", "demo/first"]
    );
    assert_eq!(
        models(&config.settings().roles.chapter),
        ["demo/second", "demo/first"]
    );
    // `bubblepaw config` prints the layered configuration, roles included.
    assert_eq!(
        config.effective().render().unwrap(),
        cli_config_file(&root).await
    );
    let store = config.open_store().unwrap();
    prepare_store(&store, config.data_dir(), config.settings()).unwrap();
    let lifetime = HostLifetime::default();
    let context = HostContext::start(config.into_request(), store, lifetime.clone(), None)
        .await
        .unwrap();
    let client = HostClient::in_process(context.ctx().clone());
    let (providers, config) = snapshot(&client).await;
    assert_eq!(providers, cli_providers);
    assert_eq!(config, cli_config);
    assert_eq!(providers["main"], "demo/first");

    // The main-model role and its live provider reload work on this context.
    client
        .call("roles.set_main", json!({"model":"demo/second"}))
        .await
        .unwrap();
    let reloaded = client.call("config.get", json!({})).await.unwrap();
    assert_eq!(reloaded["model"], "demo/second");
    client.close().await;
    lifetime.shutdown().await.unwrap();
    context.shutdown().await.unwrap();
}
