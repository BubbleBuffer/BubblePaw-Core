//! A client built without the SDK's `host` feature pairs with a host, opens it
//! through the host's own endpoint (`ws://127.0.0.1`, as a Cloudflare Tunnel
//! would publish it) and calls `session.list`. The host side is the runner with
//! its defaults, serving the CLI host's context on the scripted demo provider.
use anyhow::{Context, Result};
use bubblepaw_runner::{
    host_config::{self, ConfigOptions, HostConfig},
    host_lifetime::HostLifetime,
    remote::{RemoteReach, RemoteServeOptions, RemoteServer, SessionBackend},
    session_host::host_context::HostContext,
};
use bubblepaw_sdk::{
    Client, Profile,
    remote::{self, DeviceKind, HostPresence, Limits, RemoteConnect, RemotePair, list_hosts},
};
use serde_json::json;
use std::time::Duration;

const CONFIG: &str = "\
[model]
provider='demo'
main='demo/first'
[providers.demo]
kind='scripted'
models=[{id='first'}]
[context]
auto_summarize=false
[agents]
auto_wake=false
";

#[tokio::test]
async fn a_client_without_the_host_pairs_and_lists_sessions() -> Result<()> {
    let root = tempfile::tempdir()?;
    std::fs::write(root.path().join("config.toml"), CONFIG)?;
    let data_dir = root.path().join("data");
    let config = HostConfig::load(ConfigOptions {
        workspace: Some(root.path().to_owned()),
        data_dir: Some(data_dir.clone()),
        config_file: Some(root.path().join("config.toml")),
        home: None,
        env: Some(Vec::new()),
        overrides: Vec::new(),
    })?;
    let store = config.open_store()?;
    host_config::prepare_store(&store, &data_dir, config.settings())?;
    let lifetime = HostLifetime::default();
    let context = HostContext::start(config.into_request(), store, lifetime.clone(), None).await?;

    let reach = RemoteReach::Local {
        port: 0,
        address: None,
    };
    let options = RemoteServeOptions::in_data_dir(&data_dir, reach, "Test host".into());
    let server = RemoteServer::start(
        options,
        Some(SessionBackend::Context(Box::new(context.ctx().clone()))),
    )?;
    server.wait_online(Duration::from_secs(10)).await?;
    let address = server.address().context("an online host has an address")?;

    // The device: its own key file and saved-host store.
    let device = root.path().join("phone");
    let key_file = device.join("device.bpkey");
    let hosts = device.join("hosts.json");
    let mut offer = server.pair(Profile::Read).await?;
    let pairing = remote::pair(RemotePair {
        address: address.clone(),
        host_id: server.host_id(),
        code: offer.code().to_owned(),
        key_file: key_file.clone(),
        hosts: hosts.clone(),
        device_name: "Test phone".into(),
        device_kind: DeviceKind::Phone,
        limits: Limits::default(),
    });
    let (pending, request) = tokio::join!(pairing, offer.next());
    let pending = pending?;
    let request = request.context("the host saw a pairing request")?;
    assert_eq!(pending.comparison(), request.comparison());
    let (paired, stored) = tokio::join!(pending.finish(), request.approve(None));
    let paired = paired?;
    stored?;
    assert_eq!(paired.id, server.host_id());

    let saved = list_hosts(&hosts)?;
    assert_eq!(saved.len(), 1);
    let presence = remote::probe_host(
        &saved[0],
        &key_file,
        Limits::default(),
        Duration::from_secs(5),
    )
    .await?;
    assert_eq!(presence, HostPresence::Online);

    let client = Client::connect_remote(RemoteConnect {
        address,
        host_id: server.host_id(),
        key_file,
        hosts,
        limits: Limits::default(),
    })
    .await?;
    assert!(client.is_attached());
    let sessions = client.call("session.list", json!({})).await?;
    assert!(
        sessions.is_array(),
        "session.list returns an array: {sessions}"
    );
    client.close().await?;

    server.shutdown().await;
    context.shutdown().await?;
    lifetime.shutdown().await?;
    Ok(())
}
