//! The remote transport: a paired device's host-API client reaching the
//! in-process host context through an in-process relay and an end-to-end
//! encrypted tunnel, paired with the `full` profile like the other clients.
use bubblepaw_remote::relay::{Relay, RelayConfig};
use bubblepaw_runner::host_api::{HostCtx, Profile};
use bubblepaw_runner::host_client::HostClient;
use bubblepaw_runner::remote::{
    self, Limits, RemoteAddress, RemoteConnect, RemotePair, RemoteReach, RemoteServeOptions,
    RemoteServer, SessionBackend,
};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(60);

/// What keeps the tunnel up: the relay, the host's remote server, and the
/// directory with both sides' keys and paired stores (outside the workspace,
/// so later fixtures see the same files as on the other transports).
pub struct Parts {
    pub server: RemoteServer,
    _relay: Relay,
    _keys: tempfile::TempDir,
}

/// Serve `ctx` through a relay, pair a `full` device and connect it.
pub async fn connect(ctx: &HostCtx) -> (HostClient, Parts) {
    let keys = tempfile::tempdir().unwrap();
    let relay = Relay::bind("127.0.0.1:0".parse().unwrap(), RelayConfig::default())
        .await
        .unwrap();
    let url = format!("ws://{}", relay.local_addr());
    let options = RemoteServeOptions {
        reach: RemoteReach::Relay(url.clone()),
        key_file: keys.path().join("host.bpkey"),
        clients: keys.path().join("clients.json"),
        host_name: "Parity host".into(),
        limits: Limits::default(),
    };
    let server = RemoteServer::start(
        options,
        Some(SessionBackend::Context(Box::new(ctx.clone()))),
    )
    .unwrap();
    server.wait_online(TIMEOUT).await.unwrap();
    let device_key = keys.path().join("device.bpkey");
    let hosts = keys.path().join("hosts.json");
    let mut offer = server.pair(Profile::Full).await.unwrap();
    let pairing = remote::pair(RemotePair {
        address: RemoteAddress::Relay(url.clone()),
        host_id: server.host_id(),
        code: offer.code().to_owned(),
        key_file: device_key.clone(),
        hosts: hosts.clone(),
        device_name: "Parity device".into(),
        limits: Limits::default(),
    });
    let (pending, request) = tokio::join!(pairing, offer.next());
    let pending = pending.unwrap();
    let request = request.expect("a pairing request");
    let (finished, approved) = tokio::join!(pending.finish(), request.approve(None));
    finished.unwrap();
    approved.unwrap();
    let client = remote::connect(RemoteConnect {
        address: RemoteAddress::Relay(url),
        host_id: server.host_id(),
        key_file: device_key,
        hosts,
        limits: Limits::default(),
    })
    .await
    .unwrap();
    (
        client,
        Parts {
            server,
            _relay: relay,
            _keys: keys,
        },
    )
}
