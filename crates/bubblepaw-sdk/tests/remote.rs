//! Remote access through an in-process relay and through the host's own
//! WebSocket endpoint (`ws://127.0.0.1`, as a Cloudflare Tunnel would publish
//! it): pairing, a paired device using the host through
//! `Client::connect_remote`, profiles and revocation. The host is the CLI
//! host's own context on the scripted demo provider.
use anyhow::{Context, Result};
use bubblepaw_remote::relay::{Relay, RelayConfig};
use bubblepaw_sdk::remote::{
    self, DeviceKeyPaths, DeviceKind, HostPresence, Limits, PairedDevice, ProcessKind,
    RemoteAddress, RemoteConnect, RemoteError, RemotePair, RemoteReach, RemoteServeOptions,
    RemoteServer, SavedHost, list_hosts,
};
use bubblepaw_sdk::{Client, ClientError, ConfigOptions, HostConfig, Profile};
use serde_json::json;
use std::path::Path;
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

struct Fixture {
    root: tempfile::TempDir,
    _relay: Option<Relay>,
    address: RemoteAddress,
    host: Client,
    server: RemoteServer,
}

async fn fixture() -> Result<Fixture> {
    let relay = Relay::bind("127.0.0.1:0".parse()?, RelayConfig::default()).await?;
    let url = format!("ws://{}", relay.local_addr());
    fixture_with(RemoteReach::Relay(url), Some(relay)).await
}

/// A host serving its own endpoint on a free loopback port.
async fn direct_fixture() -> Result<Fixture> {
    let reach = RemoteReach::Local {
        port: 0,
        address: None,
    };
    fixture_with(reach, None).await
}

async fn fixture_with(reach: RemoteReach, relay: Option<Relay>) -> Result<Fixture> {
    let root = tempfile::tempdir()?;
    std::fs::write(root.path().join("config.toml"), CONFIG)?;
    let config = HostConfig::load(ConfigOptions {
        workspace: Some(root.path().to_owned()),
        data_dir: Some(root.path().join("data")),
        config_file: Some(root.path().join("config.toml")),
        home: None,
        env: Some(Vec::new()),
        overrides: Vec::new(),
    })?;
    let host = Client::embedded_configured(config).await?;
    let options =
        RemoteServeOptions::in_data_dir(&root.path().join("data"), reach, "Test host".into());
    let server = host.serve_remote(options)?;
    server.wait_online(Duration::from_secs(10)).await?;
    let address = server.address().context("an online host has an address")?;
    Ok(Fixture {
        root,
        _relay: relay,
        address,
        host,
        server,
    })
}

impl Fixture {
    fn device_dir(&self, name: &str) -> std::path::PathBuf {
        self.root.path().join(name)
    }

    fn pair_options(&self, device: &Path, code: String) -> RemotePair {
        RemotePair {
            address: self.address.clone(),
            host_id: self.server.host_id(),
            code,
            key_file: device.join("device.bpkey"),
            hosts: device.join("hosts.json"),
            device_name: "Test phone".into(),
            device_kind: DeviceKind::Phone,
            limits: Limits::default(),
        }
    }

    fn connect_options(&self, device: &Path) -> RemoteConnect {
        RemoteConnect {
            address: self.address.clone(),
            host_id: self.server.host_id(),
            key_file: device.join("device.bpkey"),
            hosts: device.join("hosts.json"),
            limits: Limits::default(),
        }
    }

    /// Pair the device in `name` with `profile`, as a person confirming the
    /// same check number on both sides would.
    async fn pair(&self, name: &str, profile: Profile) -> Result<(RemoteConnect, PairedDevice)> {
        let device = self.device_dir(name);
        let mut offer = self.server.pair(profile).await?;
        let payload = offer.qr_payload().clone();
        assert_eq!(payload.host, self.server.host_id());
        assert_eq!(payload.address(), self.address);
        let pairing = remote::pair(self.pair_options(&device, offer.code().to_owned()));
        let (pending, request) = tokio::join!(pairing, offer.next());
        let pending = pending?;
        let request = request.expect("a pairing request");
        assert_eq!(pending.comparison(), request.comparison());
        assert_eq!(pending.comparison().len(), 6);
        assert_eq!(pending.host_name(), "Test host");
        assert_eq!(request.device_name(), "Test phone");
        let (paired, stored) = tokio::join!(pending.finish(), request.approve(None));
        let paired = paired?;
        let stored = stored?;
        assert_eq!(paired.id, self.server.host_id());
        assert_eq!(stored.profile, profile);
        Ok((self.connect_options(&device), stored))
    }
}

/// A wrong code with the right shape, so the host checks it.
fn wrong(code: &str) -> String {
    let first = if code.starts_with('0') { '1' } else { '0' };
    format!("{first}{}", &code[1..])
}

#[tokio::test]
async fn paired_device_lists_sessions_and_runs_a_turn() -> Result<()> {
    lists_sessions_and_runs_a_turn(fixture().await?).await
}

#[tokio::test]
async fn direct_endpoint_pairs_and_runs_a_turn() -> Result<()> {
    let fixture = direct_fixture().await?;
    let RemoteAddress::Direct(url) = &fixture.address else {
        panic!("a local endpoint has a direct address");
    };
    assert!(url.starts_with("ws://127.0.0.1:"), "{url}");
    assert!(fixture.server.local_port().is_some());
    lists_sessions_and_runs_a_turn(fixture).await
}

async fn lists_sessions_and_runs_a_turn(fixture: Fixture) -> Result<()> {
    let (connect, device) = fixture.pair("phone", Profile::Full).await?;
    assert!(device.paired_at.is_some());
    let client = Client::connect_remote(connect).await?;
    let mut notifications = client.subscribe();
    let session = client
        .call("session.open", json!({"title":"remote"}))
        .await
        .with_context(|| "session.open")?;
    let id = session["id"].as_str().unwrap().to_owned();
    client
        .call("run.start", json!({"session_id":id,"text":"Hello"}))
        .await
        .with_context(|| "run.start")?;
    let run = client
        .call("run.wait", json!({"session_id":id}))
        .await
        .with_context(|| "run.wait")?;
    assert_eq!(run["status"], "completed", "{run}");
    let event = tokio::time::timeout(Duration::from_secs(10), notifications.recv())
        .await?
        .expect("an event notification");
    assert_eq!(event["method"], "event", "{event}");
    let list = client.call("session.list", json!({})).await?;
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .any(|session| session["id"] == id),
        "{list}"
    );
    // The host sees the same session through its own client.
    let local = fixture.host.call("session.list", json!({})).await?;
    assert_eq!(local, list);
    let mut seen = fixture.server.devices()?;
    assert_eq!(seen.len(), 1);
    assert!(seen[0].last_seen.is_some(), "a connect records last_seen");
    seen[0].last_seen = None;
    assert_eq!(seen, vec![device]);
    client.close().await?;
    fixture.server.shutdown().await;
    fixture.host.close().await?;
    Ok(())
}

#[tokio::test]
async fn pairing_needs_the_code_and_revoke_closes_the_tunnel() -> Result<()> {
    let fixture = fixture().await?;
    let device = fixture.device_dir("tablet");
    let offer = fixture.server.pair(Profile::Read).await?;
    let attempt = remote::pair(fixture.pair_options(&device, wrong(offer.code()))).await;
    assert_eq!(attempt.err(), Some(RemoteError::WrongCode));
    assert!(fixture.server.devices()?.is_empty());
    drop(offer);

    let (connect, paired) = fixture.pair("tablet", Profile::Read).await?;
    assert_eq!(fixture.server.devices()?, vec![paired.clone()]);
    let client = Client::connect_remote(connect.clone()).await?;
    client.call("session.list", json!({})).await?;
    assert!(fixture.server.revoke(&paired.id).await?);
    assert!(fixture.server.devices()?.is_empty());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match client.call("session.list", json!({})).await {
            Err(ClientError::Disconnected) => break,
            Ok(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            other => panic!("expected the revoked tunnel to close, got {other:?}"),
        }
    }
    match Client::connect_remote(connect).await {
        Err(RemoteError::Refused) => {}
        Err(other) => panic!("expected a refusal, got {other}"),
        Ok(_) => panic!("a revoked device connected"),
    }
    fixture.server.shutdown().await;
    fixture.host.close().await?;
    Ok(())
}

#[tokio::test]
async fn profiles_limit_what_a_device_may_call() -> Result<()> {
    let fixture = fixture().await?;
    let (read, _) = fixture.pair("reader", Profile::Read).await?;
    let (full, _) = fixture.pair("owner", Profile::Full).await?;
    let read = Client::connect_remote(read).await?;
    let full = Client::connect_remote(full).await?;
    let set = json!({"key":"agents.wake_debounce_ms","value":77});
    match read.call("config.set", set.clone()).await {
        Err(ClientError::Rpc { code, message, .. }) => {
            assert_eq!(code, -32003);
            assert!(message.contains("does not permit config.set"), "{message}");
        }
        other => panic!("expected the profile error, got {other:?}"),
    }
    read.call("session.list", json!({})).await?;
    full.call("config.set", set).await?;
    let value = full
        .call("config.get", json!({"key":"agents.wake_debounce_ms"}))
        .await?;
    assert!(value.to_string().contains("77"), "{value}");
    full.call(
        "config.set",
        json!({"key":"agents.wake_debounce_ms","value":null}),
    )
    .await?;
    read.close().await?;
    full.close().await?;
    fixture.server.shutdown().await;
    fixture.host.close().await?;
    Ok(())
}

#[tokio::test]
async fn last_seen_and_kind_reach_the_device_list() -> Result<()> {
    let fixture = fixture().await?;
    let (connect, paired) = fixture.pair("phone", Profile::Full).await?;
    assert_eq!(paired.kind, DeviceKind::Phone);
    assert_eq!(paired.last_seen, None);
    let hosts_file = connect.hosts.clone();
    let client = Client::connect_remote(connect).await?;
    client.call("session.list", json!({})).await?;
    let devices = fixture.server.devices()?;
    assert_eq!(devices[0].kind, DeviceKind::Phone);
    assert!(devices[0].last_seen.is_some());
    // The device side records the connection too.
    let saved = list_hosts(&hosts_file)?;
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].id, fixture.server.host_id());
    assert_eq!(saved[0].label, "Test host");
    assert_eq!(saved[0].profile, Some(Profile::Full));
    assert!(saved[0].paired_at.is_some());
    assert!(saved[0].last_connected.is_some());
    client.close().await?;
    fixture.server.shutdown().await;
    fixture.host.close().await?;
    Ok(())
}

#[tokio::test]
async fn a_second_serve_is_refused_and_status_names_the_holder() -> Result<()> {
    let fixture = fixture().await?;
    let data = fixture.root.path().join("data");
    let paths = remote::RemotePaths::in_data_dir(&data);
    let status = remote::serve_status(&paths);
    assert!(status.shared);
    assert_eq!(status.pid, Some(std::process::id()));
    assert_eq!(status.process, Some(ProcessKind::Embedded));
    assert_eq!(status.host_id, Some(fixture.server.host_id()));
    let again = RemoteServeOptions::in_data_dir(
        &data,
        RemoteReach::Local {
            port: 0,
            address: None,
        },
        "Second".into(),
    );
    let error = fixture.host.serve_remote(again).err();
    assert!(error.is_some(), "a second serve must be refused");
    assert!(
        error
            .unwrap()
            .to_string()
            .contains("is serving this data directory")
    );
    // Changing the device key is refused while sharing.
    let key = DeviceKeyPaths::from(&paths);
    assert!(remote::regenerate_device_key(&key).is_err());
    fixture.server.shutdown().await;
    assert!(!remote::serve_status(&paths).shared);
    fixture.host.close().await?;
    Ok(())
}

#[tokio::test]
async fn saved_hosts_list_forget_and_probe_through_the_relay() -> Result<()> {
    let fixture = fixture().await?;
    let (connect, _) = fixture.pair("phone", Profile::Read).await?;
    let hosts_file = connect.hosts.clone();
    let key_file = connect.key_file.clone();
    let host: SavedHost = list_hosts(&hosts_file)?.remove(0);
    assert_eq!(host.address, fixture.address);
    let wait = Duration::from_secs(5);
    let online = remote::probe_host(&host, &key_file, Limits::default(), wait).await?;
    assert_eq!(online, HostPresence::Online);
    let host_id = fixture.server.host_id();
    fixture.server.shutdown().await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let now = remote::probe_host(&host, &key_file, Limits::default(), wait).await?;
        if now == HostPresence::Offline {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "still online");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(remote::forget_host(&hosts_file, &host_id)?);
    assert!(!remote::forget_host(&hosts_file, &host_id)?);
    assert!(list_hosts(&hosts_file)?.is_empty());
    fixture.host.close().await?;
    Ok(())
}

#[tokio::test]
async fn a_direct_probe_reads_online_then_offline() -> Result<()> {
    let fixture = direct_fixture().await?;
    let (connect, _) = fixture.pair("phone", Profile::Read).await?;
    let host = list_hosts(&connect.hosts)?.remove(0);
    let wait = Duration::from_secs(2);
    let probe = || remote::probe_host(&host, &connect.key_file, Limits::default(), wait);
    assert_eq!(probe().await?, HostPresence::Online);
    fixture.server.shutdown().await;
    assert_eq!(probe().await?, HostPresence::Offline);
    fixture.host.close().await?;
    Ok(())
}

#[tokio::test]
async fn the_device_key_exports_imports_and_regenerates() -> Result<()> {
    let fixture = fixture().await?;
    let (connect, paired) = fixture.pair("phone", Profile::Full).await?;
    let data = fixture.root.path().join("data");
    let paths = remote::RemotePaths::in_data_dir(&data);
    let key = DeviceKeyPaths::from(&paths);
    let before = remote::device_key_info(&key.key_file)?;
    assert_eq!(before.id, fixture.server.host_id());
    assert_eq!(before.fingerprint.len(), 64);
    let file = fixture.root.path().join("exported.bpkey");
    remote::export_device_key(&key.key_file, &file, "correct horse")?;
    assert!(remote::export_device_key(&key.key_file, &file, "correct horse").is_err());
    assert!(remote::export_device_key(&key.key_file, &file.with_extension("x"), "").is_err());
    fixture.server.shutdown().await;

    // A wrong passphrase changes nothing; the right one keeps the same ID and
    // every pairing.
    assert!(remote::import_device_key(&key, &file, Some("wrong")).is_err());
    assert!(remote::import_device_key(&key, &file, None).is_err());
    let same = remote::import_device_key(&key, &file, Some("correct horse"))?;
    assert_eq!(same.id, before.id);
    assert_eq!((same.cleared_devices, same.cleared_hosts), (0, 0));
    assert_eq!(remote::list_devices(&key.clients)?.len(), 1);

    // Regenerating changes the ID and clears both directions.
    let change = remote::regenerate_device_key(&key)?;
    assert_eq!(change.old_id.as_deref(), Some(before.id.as_str()));
    assert_ne!(change.id, before.id);
    assert_eq!(change.cleared_devices, 1);
    assert!(remote::list_devices(&key.clients)?.is_empty());
    assert_eq!(remote::device_key_info(&key.key_file)?.id, change.id);

    // The old pairing is refused: the host now has another ID and does not
    // know the device.
    let options = RemoteServeOptions::in_data_dir(
        &data,
        RemoteReach::Relay(match &fixture.address {
            RemoteAddress::Relay(url) => url.clone(),
            other => panic!("expected a relay address, got {other:?}"),
        }),
        "Test host".into(),
    );
    let server = fixture.host.serve_remote(options)?;
    server.wait_online(Duration::from_secs(10)).await?;
    assert_eq!(server.host_id(), change.id);
    assert_ne!(server.host_id(), paired.id);
    match Client::connect_remote(connect).await {
        Err(_) => {}
        Ok(_) => panic!("a device paired with the old key connected"),
    }
    server.shutdown().await;
    fixture.host.close().await?;
    Ok(())
}
