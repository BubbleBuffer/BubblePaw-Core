use bubblepaw_remote::{
    Error,
    device::{DeviceId, DeviceKey},
    relay::{self, Limits, Relay, RelayAdmission, RelayConfig, RelayProof, RelayRouter},
    tunnel,
};
use futures_util::{SinkExt, StreamExt};
use std::{
    collections::HashMap,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::{TcpListener, TcpStream},
    task::JoinSet,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        handshake::server::{Request, Response},
        http,
        protocol::Role,
    },
};

struct AccountAdmission {
    account: &'static str,
    owners: Arc<HashMap<DeviceId, &'static str>>,
    calls: Arc<AtomicUsize>,
}

impl RelayAdmission for AccountAdmission {
    fn admit_host(&self, host: &DeviceId, proof: &RelayProof) -> bool {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(*host == DeviceId::from_public_key(&proof.public_key));
        self.owners.get(host) == Some(&self.account)
    }
    fn admit_client(&self, client: &DeviceId, proof: &RelayProof) -> bool {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(*client == DeviceId::from_public_key(&proof.public_key));
        self.owners.get(client) == Some(&self.account)
    }
    fn may_connect(&self, client: &DeviceId, host: &DeviceId, _proof: &RelayProof) -> bool {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.owners.get(client) == Some(&self.account)
            && self.owners.get(host) == Some(&self.account)
    }
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn authenticated_socket(url: &str, token: &'static str, limits: &Limits) -> Socket {
    let mut request = url.into_client_request().unwrap();
    request.headers_mut().insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_static(token),
    );
    tokio_tungstenite::connect_async_with_config(request, Some(limits.websocket_config()), false)
        .await
        .unwrap()
        .0
}

#[tokio::test]
async fn mounted_route_binds_authenticated_account_to_verified_device_proof() {
    let host_key = DeviceKey::from_seed(&[41; 32]);
    let alice_key = DeviceKey::from_seed(&[42; 32]);
    let bob_key = DeviceKey::from_seed(&[43; 32]);
    let owners = Arc::new(HashMap::from([
        (host_key.id(), "alice"),
        (alice_key.id(), "alice"),
        (bob_key.id(), "bob"),
    ]));
    let calls = Arc::new(AtomicUsize::new(0));
    let router = RelayRouter::new(RelayConfig::default()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/scratch/relay", listener.local_addr().unwrap());
    let shared_router = router.clone();
    let policy_calls = calls.clone();
    let server = tokio::spawn(async move {
        let mut tasks = JoinSet::new();
        for _ in 0..5 {
            let (stream, peer) = listener.accept().await.unwrap();
            let router = shared_router.clone();
            let owners = owners.clone();
            let calls = policy_calls.clone();
            tasks.spawn(async move {
                let account = Arc::new(OnceLock::new());
                let authenticated = account.clone();
                // Synthetic HTTP-session fixture. A real hub verifies its OAuth
                // session here and binds that result to this specific upgrade.
                // Tungstenite requires its unboxed HTTP error response here.
                #[expect(clippy::result_large_err)]
                let callback = move |request: &Request, response: Response| {
                    let account = match request
                        .headers()
                        .get(http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                    {
                        Some("Bearer fixture-alice") => "alice",
                        Some("Bearer fixture-bob") => "bob",
                        _ => {
                            return Err(http::Response::builder()
                                .status(401)
                                .body(Some("unauthenticated".into()))
                                .unwrap());
                        }
                    };
                    let _ = authenticated.set(account);
                    Ok(response)
                };
                let ws = tokio_tungstenite::accept_hdr_async_with_config(
                    stream,
                    callback,
                    Some(router.websocket_config()),
                )
                .await
                .unwrap();
                let admission = AccountAdmission {
                    account: account.get().unwrap(),
                    owners,
                    calls,
                };
                let _ = router
                    .serve_websocket(ws, peer.ip(), Arc::new(admission))
                    .await;
            });
        }
        while tasks.join_next().await.is_some() {}
    });
    let limits = Limits::default();
    // Owning an account session does not grant possession of another account's device.
    let wrong_account = authenticated_socket(&url, "Bearer fixture-bob", &limits).await;
    assert!(
        relay::register_socket(wrong_account, &host_key, limits.clone())
            .await
            .is_err()
    );
    let host_socket = authenticated_socket(&url, "Bearer fixture-alice", &limits).await;
    let mut host = relay::register_socket(host_socket, &host_key, limits.clone())
        .await
        .unwrap();
    let bob_socket = authenticated_socket(&url, "Bearer fixture-bob", &limits).await;
    assert!(
        relay::connect_socket(bob_socket, &bob_key, &host_key.id(), limits.clone())
            .await
            .is_err()
    );
    let alice_socket = authenticated_socket(&url, "Bearer fixture-alice", &limits).await;
    let target = host_key.id();
    let (accepted, connected) = tokio::join!(
        host.accept(),
        relay::connect_socket(alice_socket, &alice_key, &target, limits.clone())
    );
    let host_trusted = [alice_key.public_key()];
    let client_trusted = [host_key.public_key()];
    let (accepted, connected) = tokio::join!(
        tunnel::accept(accepted.unwrap(), &host_key, &host_trusted),
        tunnel::connect(connected.unwrap(), &alice_key, &client_trusted)
    );
    let mut accepted = accepted.unwrap();
    let mut connected = connected.unwrap();
    connected
        .send(b"account admitted encrypted session")
        .await
        .unwrap();
    assert_eq!(
        accepted.recv().await.unwrap(),
        b"account admitted encrypted session"
    );
    let before_forgery = calls.load(Ordering::SeqCst);
    let mut forged = authenticated_socket(&url, "Bearer fixture-alice", &limits).await;
    forged.next().await.unwrap().unwrap();
    let mut frame = vec![2];
    frame.extend_from_slice(&host_key.public_key());
    frame.extend_from_slice(&[0; 64]);
    forged.send(Message::Binary(frame.into())).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), forged.next())
            .await
            .unwrap()
            .is_none_or(|result| result.is_err() || matches!(result, Ok(Message::Close(_))))
    );
    assert_eq!(calls.load(Ordering::SeqCst), before_forgery);
    drop(accepted);
    drop(connected);
    drop(host);
    router.shutdown().await;
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn supplied_socket_path_enforces_configuration_ip_limits_and_shutdown() {
    let limits = Limits {
        connections_per_ip: 1,
        ..Default::default()
    };
    let router = RelayRouter::new(RelayConfig {
        limits: limits.clone(),
        ..Default::default()
    })
    .unwrap();
    let ip = "127.0.0.1".parse().unwrap();
    let (server, _client) = tokio::io::duplex(1024);
    let unbounded = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
    assert!(matches!(
        router
            .serve_websocket(unbounded, ip, Arc::new(relay::PermissiveAdmission))
            .await,
        Err(Error::Protocol(_))
    ));
    let (server, client) = tokio::io::duplex(1024);
    let server =
        WebSocketStream::from_raw_socket(server, Role::Server, Some(limits.websocket_config()))
            .await;
    let mut client =
        WebSocketStream::from_raw_socket(client, Role::Client, Some(limits.websocket_config()))
            .await;
    let first_router = router.clone();
    let serving = tokio::spawn(async move {
        first_router
            .serve_websocket(server, ip, Arc::new(relay::PermissiveAdmission))
            .await
    });
    client.next().await.unwrap().unwrap(); // The first reservation is now active.
    let (server, _client) = tokio::io::duplex(1024);
    let second =
        WebSocketStream::from_raw_socket(server, Role::Server, Some(limits.websocket_config()))
            .await;
    assert!(matches!(
        router
            .serve_websocket(second, ip, Arc::new(relay::PermissiveAdmission))
            .await,
        Err(Error::Limit("connections per IP"))
    ));
    tokio::time::timeout(Duration::from_secs(2), router.shutdown())
        .await
        .unwrap();
    assert!(serving.await.unwrap().is_err());
}

#[tokio::test]
async fn shutdown_interrupts_pending_upgrade_and_blocked_websocket_write() {
    let relay = Relay::bind("127.0.0.1:0".parse().unwrap(), RelayConfig::default())
        .await
        .unwrap();
    let _pending_upgrade = TcpStream::connect(relay.local_addr()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), relay.shutdown())
        .await
        .unwrap();

    let router = RelayRouter::new(RelayConfig::default()).unwrap();
    let (server, mut client) = tokio::io::duplex(1);
    let socket =
        WebSocketStream::from_raw_socket(server, Role::Server, Some(router.websocket_config()))
            .await;
    let serving_router = router.clone();
    let serving = tokio::spawn(async move {
        serving_router
            .serve_websocket(
                socket,
                "127.0.0.1".parse().unwrap(),
                Arc::new(relay::PermissiveAdmission),
            )
            .await
    });
    // Seeing a frame byte proves that the challenge write started. The tiny
    // unread transport then blocks its remaining bytes, with no idle-time wait.
    tokio::io::AsyncReadExt::read_u8(&mut client).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), router.shutdown())
        .await
        .unwrap();
    assert!(serving.await.unwrap().is_err());
}
