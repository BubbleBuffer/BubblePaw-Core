//! Cloudflare Tunnel reach with a fake `cloudflared` and a local fake
//! Cloudflare API: the quick tunnel's address is read from its log and
//! changes on restart; domain setup creates the tunnel and its DNS route once
//! and runs it with the stored credentials.
#[path = "remote_tunnel/fake.rs"]
mod fake;
#[path = "remote_tunnel/fake_api.rs"]
mod fake_api;

use bubblepaw_runner::host_api::Profile;
use bubblepaw_runner::remote::{
    CloudflareApi, CloudflareTunnel, RemoteAddress, RemoteReach, RemoteServeOptions, RemoteServer,
    RemoteStatus, TunnelMode,
};
use fake_api::{FakeApi, TOKEN, TUNNEL_ID};
use serde_json::json;
use std::path::Path;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(30);
const REGISTERED: &str =
    "2026-11-02T10:00:00Z INF Registered tunnel connection connIndex=0 location=fra01";

fn start(root: &Path, tunnel: CloudflareTunnel) -> RemoteServer {
    let options = RemoteServeOptions::in_data_dir(
        &root.join("data"),
        RemoteReach::Cloudflare(tunnel),
        "Tunnel host".into(),
    );
    RemoteServer::start(options, None).unwrap()
}

fn tunnel(root: &Path, mode: TunnelMode, program: &Path) -> CloudflareTunnel {
    CloudflareTunnel {
        mode,
        port: 0,
        cloudflared: program.to_owned(),
        dir: root.join("data/remote/cloudflare"),
        api: None,
    }
}

async fn wait_for_address(server: &RemoteServer, url: &str) {
    let mut address = server.watch_address();
    let wanted = Some(RemoteAddress::Direct(url.to_owned()));
    tokio::time::timeout(WAIT, address.wait_for(|current| *current == wanted))
        .await
        .unwrap_or_else(|_| panic!("no address {url}; last error {:?}", server.last_error()))
        .unwrap();
}

#[tokio::test]
async fn a_quick_tunnel_publishes_its_address_and_gets_a_new_one_on_restart() {
    let root = tempfile::tempdir().unwrap();
    let fake = fake::install(
        &root.path().join("bin"),
        &[
            "INF Requesting new quick Tunnel on trycloudflare.com...",
            "INF |  https://run-{n}.trycloudflare.com  |",
        ],
        Some(1500),
    );
    let server = start(
        root.path(),
        tunnel(root.path(), TunnelMode::Quick, &fake.program),
    );
    let port = server.local_port().expect("a local endpoint");
    wait_for_address(&server, "wss://run-1.trycloudflare.com").await;
    // The first run exits; the restarted one has a new address.
    wait_for_address(&server, "wss://run-2.trycloudflare.com").await;
    assert_eq!(server.status(), RemoteStatus::Online);
    let offer = server.pair(Profile::Read).await.unwrap();
    let payload = offer.qr_payload();
    assert_eq!(
        payload.direct.as_deref(),
        Some("wss://run-2.trycloudflare.com")
    );
    assert_eq!(payload.relay, None);
    drop(offer);
    let origin = format!("http://127.0.0.1:{port}");
    let expected: Vec<String> = ["--no-autoupdate", "tunnel", "--url", origin.as_str()]
        .map(str::to_owned)
        .to_vec();
    assert_eq!(fake.runs(), vec![expected.clone(), expected]);
    server.shutdown().await;
}

#[tokio::test]
async fn a_missing_trycloudflare_address_leaves_the_host_offline() {
    let root = tempfile::tempdir().unwrap();
    let fake = fake::install(
        &root.path().join("bin"),
        &["ERR failed to request"],
        Some(200),
    );
    let server = start(
        root.path(),
        tunnel(root.path(), TunnelMode::Quick, &fake.program),
    );
    let mut status = server.watch_status();
    tokio::time::timeout(WAIT, status.wait_for(|s| *s == RemoteStatus::Offline))
        .await
        .unwrap()
        .unwrap();
    assert!(server.address().is_none());
    let error = server.last_error().unwrap();
    assert!(error.contains("cloudflared exited"), "{error}");
    let state = server.share_state();
    assert_eq!(state.last_error.as_deref(), Some(error.as_str()));
    let failure = state.failure.unwrap();
    assert_eq!(failure.transport, "cloudflared");
    assert_eq!(failure.exit_code, Some(1));
    assert_eq!(failure.retry_after_secs, 1);
    assert!(server.pair(Profile::Read).await.is_err());
    server.shutdown().await;
}

fn domain(root: &Path, program: &Path, api: Option<&FakeApi>) -> CloudflareTunnel {
    let mode = TunnelMode::Domain {
        hostname: "paw.example.com".into(),
    };
    let mut tunnel = tunnel(root, mode, program);
    tunnel.api = api.map(|api| {
        let mut access = CloudflareApi::new(TOKEN.into());
        access.base_url = api.url.clone();
        access
    });
    tunnel
}

#[tokio::test]
async fn a_domain_tunnel_is_set_up_through_the_api_once() {
    let root = tempfile::tempdir().unwrap();
    let api = FakeApi::start(Vec::new()).await;
    let fake = fake::install(&root.path().join("bin"), &[REGISTERED], None);
    let server = start(root.path(), domain(root.path(), &fake.program, Some(&api)));
    server.wait_online(WAIT).await.unwrap();
    let port = server.local_port().unwrap();
    assert_eq!(
        server.address(),
        Some(RemoteAddress::Direct("wss://paw.example.com".into()))
    );
    let dir = root.path().join("data/remote/cloudflare");
    let credentials: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("tunnel.json")).unwrap()).unwrap();
    {
        let recorded = api.state.lock().unwrap();
        assert_eq!(recorded.secrets.len(), 1);
        assert_eq!(credentials["TunnelSecret"], recorded.secrets[0].as_str());
        assert_eq!(recorded.records.len(), 1);
        let record = &recorded.records[0];
        assert_eq!(record["type"], "CNAME");
        assert_eq!(record["name"], "paw.example.com");
        assert_eq!(record["content"], format!("{TUNNEL_ID}.cfargotunnel.com"));
        assert_eq!(record["proxied"], true);
    }
    assert_eq!(credentials["TunnelID"], TUNNEL_ID);
    assert_eq!(credentials["AccountTag"], "acct-1");
    let credentials_path = dir.join("tunnel.json").to_string_lossy().into_owned();
    let origin = format!("http://127.0.0.1:{port}");
    let run: Vec<String> = [
        "--no-autoupdate",
        "tunnel",
        "run",
        "--credentials-file",
        credentials_path.as_str(),
        "--url",
        origin.as_str(),
        TUNNEL_ID,
    ]
    .map(str::to_owned)
    .to_vec();
    assert_eq!(fake.runs(), vec![run]);
    server.shutdown().await;

    // A second start reuses the tunnel and its route.
    let server = start(root.path(), domain(root.path(), &fake.program, Some(&api)));
    server.wait_online(WAIT).await.unwrap();
    assert_eq!(api.state.lock().unwrap().secrets.len(), 1);
    assert_eq!(api.state.lock().unwrap().records.len(), 1);
    server.shutdown().await;
}

#[tokio::test]
async fn domain_setup_leaves_other_dns_records_alone() {
    let root = tempfile::tempdir().unwrap();
    let existing =
        json!({"id": "record-a", "type": "A", "name": "paw.example.com", "content": "192.0.2.1"});
    let api = FakeApi::start(vec![existing.clone()]).await;
    let fake = fake::install(&root.path().join("bin"), &[REGISTERED], None);
    let server = start(root.path(), domain(root.path(), &fake.program, Some(&api)));
    let mut status = server.watch_status();
    tokio::time::timeout(WAIT, status.wait_for(|s| *s == RemoteStatus::Offline))
        .await
        .unwrap()
        .unwrap();
    let error = server.last_error().unwrap();
    assert!(
        error.contains("do not point at a Cloudflare Tunnel"),
        "{error}"
    );
    assert!(!error.contains(TOKEN));
    assert_eq!(api.state.lock().unwrap().records, vec![existing]);
    // cloudflared never ran a tunnel.
    assert!(fake.runs().is_empty());
    server.shutdown().await;
}

#[tokio::test]
async fn without_a_token_domain_setup_uses_the_cloudflared_login() {
    let root = tempfile::tempdir().unwrap();
    let fake = fake::install(&root.path().join("bin"), &[REGISTERED], None);
    let server = start(root.path(), domain(root.path(), &fake.program, None));
    server.wait_online(WAIT).await.unwrap();
    let runs = fake.runs();
    assert_eq!(runs.len(), 3, "{runs:?}");
    assert_eq!(runs[0][..3], ["tunnel", "create", "--credentials-file"]);
    assert_eq!(runs[0][4], "bubblepaw-paw-example-com");
    assert_eq!(
        runs[1],
        [
            "tunnel",
            "route",
            "dns",
            fake_tunnel_id(),
            "paw.example.com"
        ]
    );
    assert_eq!(runs[2][..3], ["--no-autoupdate", "tunnel", "run"]);
    server.shutdown().await;
}

/// The tunnel ID the fake's `tunnel create` writes.
fn fake_tunnel_id() -> &'static str {
    "6ff42ae2-765d-4adf-8112-31c55c1551ef"
}
