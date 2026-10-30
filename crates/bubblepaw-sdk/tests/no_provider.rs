//! A CLI host starts with no usable provider so an app can configure one
//! through the host API. Model work is refused with a typed error until then;
//! `Client::is_attached` tells attach from embed.
use anyhow::Result;
use bubblepaw_sdk::{Client, ClientError, ConfigOptions, HostBuilder, HostConfig};
use bubblepaw_sdk::{
    providers::{ProviderRegistry, ScriptedProvider},
    store::Store,
};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};

/// A first run: no provider in the config, no credentials in the environment
/// and no Codex login. The file only quiets background work the tests do not
/// need and keeps the scripted provider read-only.
fn first_run(root: &Path) -> ConfigOptions {
    let config = root.join("config.toml");
    std::fs::write(
        &config,
        "[context]
auto_summarize=false
[agents]
auto_wake=false
[approvals]
read_only=true
",
    )
    .unwrap();
    ConfigOptions {
        workspace: Some(root.to_owned()),
        data_dir: Some(root.join("data")),
        config_file: Some(config),
        home: Some(root.join("home")),
        env: Some(Vec::new()),
        overrides: Vec::new(),
    }
}

async fn open_session(client: &Client) -> Result<String> {
    let session = client.call("session.open", json!({})).await?;
    Ok(session["id"].as_str().unwrap().to_owned())
}

/// The typed error, with its message checked for the config-key wording.
fn assert_no_provider(result: Result<Value, ClientError>) {
    match result {
        Err(ClientError::NoProviderConfigured { message }) => {
            for key in [
                "model.provider",
                "providers.<id>",
                "providers.save",
                "roles.set_main",
            ] {
                assert!(message.contains(key), "{key} missing from: {message}");
            }
            assert!(!message.contains("--"), "flag text in: {message}");
        }
        other => panic!("expected NoProviderConfigured, got {other:?}"),
    }
}

#[tokio::test]
async fn a_host_without_a_provider_starts_serves_config_and_refuses_model_work() -> Result<()> {
    let root = tempfile::tempdir()?;
    let config = HostConfig::load(first_run(root.path()))?;
    let owner = Client::embedded_configured(config).await?;

    let providers = owner.call("providers.list", json!({})).await?;
    assert!(
        providers["main_unavailable"]
            .as_str()
            .is_some_and(|reason| reason.contains("providers.save")),
        "{providers}"
    );
    assert!(owner.call("config.get", json!({})).await?["model"].is_string());
    let id = open_session(&owner).await?;
    assert_no_provider(
        owner
            .call("run.start", json!({"session_id":id,"text":"hello"}))
            .await,
    );
    assert_no_provider(owner.call("run.resume", json!({"session_id":id})).await);
    assert_no_provider(
        owner
            .call("session.compact", json!({"session_id":id}))
            .await,
    );
    // The same typed error crosses the published endpoint.
    let attached = Client::attach(root.path().join("data")).await?;
    assert_no_provider(
        attached
            .call("run.start", json!({"session_id":id,"text":"hello"}))
            .await,
    );
    attached.close().await?;
    owner.close().await?;
    Ok(())
}

#[tokio::test]
async fn configuring_a_provider_through_the_host_api_enables_runs_without_a_restart() -> Result<()>
{
    let root = tempfile::tempdir()?;
    let owner = Client::embedded_configured(HostConfig::load(first_run(root.path()))?).await?;
    let id = open_session(&owner).await?;
    assert_no_provider(
        owner
            .call("run.start", json!({"session_id":id,"text":"hello"}))
            .await,
    );

    owner
        .call(
            "providers.save",
            json!({"name":"demo","input":{"kind":"scripted","models":[{"id":"first"}]}}),
        )
        .await?;
    // Saving a provider leaves the main route open until it is selected.
    assert_no_provider(
        owner
            .call("run.start", json!({"session_id":id,"text":"hello"}))
            .await,
    );
    let selected = owner
        .call("roles.set_main", json!({"model":"demo/first"}))
        .await?;
    assert_eq!(selected["main_unavailable"], Value::Null, "{selected}");

    owner
        .call("run.start", json!({"session_id":id,"text":"hello"}))
        .await?;
    let run = owner.call("run.wait", json!({"session_id":id})).await?;
    assert_eq!(run["status"], "completed", "{run}");
    owner.close().await?;
    Ok(())
}

#[tokio::test]
async fn is_attached_tells_attaching_from_embedding() -> Result<()> {
    let root = tempfile::tempdir()?;
    let config = || HostConfig::load(first_run(root.path()));
    let owner = Client::open_configured(config()?).await?;
    assert!(!owner.is_attached(), "the first client embeds the host");
    let second = Client::open_configured(config()?).await?;
    assert!(second.is_attached(), "the owner's host is already running");
    let direct = Client::attach(root.path().join("data")).await?;
    assert!(direct.is_attached());
    direct.close().await?;
    second.close().await?;
    owner.close().await?;

    let workspace = tempfile::tempdir()?;
    let builder = HostBuilder::new(
        Store::in_memory()?,
        ProviderRegistry::new("scripted", Arc::new(ScriptedProvider::new("done"))),
    )
    .workspace(workspace.path());
    let embedded = Client::embedded(builder).await?;
    assert!(!embedded.is_attached());
    embedded.close().await?;
    Ok(())
}
