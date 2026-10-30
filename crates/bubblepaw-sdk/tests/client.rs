//! `Client`: embedded, attached to an embedded host, and `open` choosing
//! between the two. Every call goes through the host-API table.
use anyhow::Result;
use bubblepaw_sdk::harness::HarnessConfig;
use bubblepaw_sdk::providers::{ProviderRegistry, ScriptedProvider};
use bubblepaw_sdk::store::Store;
use bubblepaw_sdk::{Client, ClientError, HostBuilder};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};

fn builder(store: Store, workspace: &Path) -> HostBuilder {
    let providers = ProviderRegistry::new(
        "scripted",
        Arc::new(ScriptedProvider::with_steps(Vec::new(), "Done.")),
    );
    HostBuilder::new(store, providers)
        .workspace(workspace)
        .history_tools(true)
        .todo(true)
        .config(HarnessConfig {
            model: "scripted".into(),
            ..Default::default()
        })
}

fn ids(list: &Value) -> Vec<String> {
    let mut ids: Vec<String> = list
        .as_array()
        .unwrap_or_else(|| panic!("session.list returns an array: {list}"))
        .iter()
        .map(|session| session["id"].as_str().unwrap().to_owned())
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn embedded_client_calls_the_table_and_streams_events() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let client = Client::embedded(builder(Store::in_memory()?, workspace.path())).await?;
    assert!(client.host().is_some());
    let mut notifications = client.subscribe();
    let session = client
        .call("session.open", json!({"title":"embedded"}))
        .await?;
    let id = session["id"].as_str().unwrap().to_owned();
    client
        .call("run.start", json!({"session_id":id,"text":"Hello"}))
        .await?;
    let run = client.call("run.wait", json!({"session_id":id})).await?;
    assert_eq!(run["status"], "completed", "{run}");
    let event = tokio::time::timeout(Duration::from_secs(10), notifications.recv())
        .await?
        .expect("an event notification");
    assert_eq!(event["method"], "event", "{event}");
    assert_eq!(
        ids(&client.call("session.list", json!({})).await?),
        vec![id]
    );
    match client.call("no.such.method", json!({})).await {
        Err(ClientError::Rpc { message, .. }) => assert!(message.contains("unknown method")),
        other => panic!("expected an RPC error, got {other:?}"),
    }
    client.close().await?;
    Ok(())
}

#[tokio::test]
async fn embedded_host_publishes_its_endpoint_for_attached_clients() -> Result<()> {
    let root = tempfile::tempdir()?;
    let data = root.path().join("data");
    let embedded = Client::embedded(builder(Store::open_data_dir(&data)?, root.path())).await?;
    assert!(data.join("host.json").exists(), "host.json published");
    embedded
        .call("session.open", json!({"title":"shared"}))
        .await?;

    let attached = Client::attach(&data).await?;
    assert!(attached.host().is_none());
    let seen = ids(&attached.call("session.list", json!({})).await?);
    assert_eq!(seen.len(), 1);
    assert_eq!(seen, ids(&embedded.call("session.list", json!({})).await?));

    // A session opened through the attached client is the embedded host's too.
    attached
        .call("session.open", json!({"title":"from attached"}))
        .await?;
    assert_eq!(
        ids(&embedded.call("session.list", json!({})).await?).len(),
        2
    );
    attached.close().await?;
    embedded
        .call("session.list", json!({}))
        .await
        .expect("closing an attached client leaves the host running");
    embedded.close().await?;
    assert!(
        !data.join("host.json").exists(),
        "host.json removed on close"
    );
    Ok(())
}

#[tokio::test]
async fn open_embeds_when_free_and_attaches_when_owned() -> Result<()> {
    let root = tempfile::tempdir()?;
    let data = root.path().join("data");
    let workspace = root.path().to_owned();
    let owner = Client::open(&data, |store| Ok(builder(store, &workspace))).await?;
    assert!(owner.host().is_some(), "a free data dir is embedded");
    owner.call("session.open", json!({"title":"owned"})).await?;

    let second = Client::open(&data, |_| -> Result<HostBuilder> {
        panic!("an owned data dir must not build a second host")
    })
    .await?;
    assert!(second.host().is_none(), "an owned data dir is attached");
    assert_eq!(
        ids(&second.call("session.list", json!({})).await?),
        ids(&owner.call("session.list", json!({})).await?)
    );
    second.close().await?;
    owner.close().await?;

    // Once the owner has stopped, the next open embeds again.
    let reopened = Client::open(&data, |store| Ok(builder(store, &workspace))).await?;
    assert!(reopened.host().is_some());
    assert_eq!(
        ids(&reopened.call("session.list", json!({})).await?).len(),
        1
    );
    reopened.close().await?;
    Ok(())
}
