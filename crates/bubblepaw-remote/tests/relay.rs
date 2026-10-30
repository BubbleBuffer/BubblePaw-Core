use bubblepaw_remote::{
    device::DeviceKey,
    frame::FrameIo,
    relay::{self, Limits, Relay, RelayConfig},
    tunnel,
};
use futures_util::{SinkExt, StreamExt};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_tungstenite::tungstenite::Message;

fn key(seed: u8) -> DeviceKey {
    DeviceKey::from_seed(&[seed; 32])
}

async fn start(config: RelayConfig) -> Relay {
    Relay::bind("127.0.0.1:0".parse().unwrap(), config)
        .await
        .unwrap()
}

#[tokio::test]
async fn two_clients_exchange_independent_encrypted_messages_through_one_registration() {
    let observed = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let capture = observed.clone();
    let relay = start(RelayConfig {
        ciphertext_observer: Some(Arc::new(move |bytes| {
            capture.lock().unwrap().push(bytes.to_vec())
        })),
        ..Default::default()
    })
    .await;
    let url = format!("ws://{}", relay.local_addr());
    let host_key = key(1);
    let host_id = host_key.id();
    let limits = Limits::default();
    let mut host = relay::register(&url, &host_key, limits.clone())
        .await
        .unwrap();
    let first_marker = b"PRIVATE CLIENT ONE MESSAGE";
    let second_marker = b"PRIVATE CLIENT TWO MESSAGE";
    let first = first_marker.repeat(6000);
    let second = second_marker.repeat(11);
    let clients = async {
        let one = async {
            let client_key = key(2);
            let io = relay::connect(&url, &client_key, &host_id, limits.clone())
                .await
                .unwrap();
            let mut encrypted = tunnel::connect(io, &client_key, &[host_key.public_key()])
                .await
                .unwrap();
            encrypted.send(&first).await.unwrap();
            assert_eq!(encrypted.recv().await.unwrap(), first);
        };
        let two = async {
            let client_key = key(3);
            let io = relay::connect(&url, &client_key, &host_id, limits.clone())
                .await
                .unwrap();
            let mut encrypted = tunnel::connect(io, &client_key, &[host_key.public_key()])
                .await
                .unwrap();
            encrypted.send(&second).await.unwrap();
            assert_eq!(encrypted.recv().await.unwrap(), second);
        };
        tokio::join!(one, two);
    };
    let server = async {
        let first = host.accept().await.unwrap();
        let second = host.accept().await.unwrap();
        assert_ne!(first.session_id(), second.session_id());
        let echo = |io| async {
            let trusted = [key(2).public_key(), key(3).public_key()];
            let mut encrypted = tunnel::accept(io, &host_key, &trusted).await.unwrap();
            let bytes = encrypted.recv().await.unwrap();
            encrypted.send(&bytes).await.unwrap();
            // Keep the transport alive until its queued response has reached the client.
            encrypted
        };
        tokio::join!(echo(first), echo(second))
    };
    let (_, transports) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(clients, server)
    })
    .await
    .unwrap();
    drop(transports);
    {
        let captured = observed.lock().unwrap();
        assert!(captured.len() >= 16);
        for bytes in captured.iter() {
            assert!(
                !bytes
                    .windows(first_marker.len())
                    .any(|window| window == first_marker)
            );
            assert!(
                !bytes
                    .windows(second_marker.len())
                    .any(|window| window == second_marker)
            );
        }
    }
    drop(host);
    relay.shutdown().await;
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn socket(url: &str) -> (Socket, Vec<u8>) {
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let challenge = socket.next().await.unwrap().unwrap().into_data().to_vec();
    assert_eq!(challenge[0], 1);
    (socket, challenge[1..].to_vec())
}

async fn auth(socket: &mut Socket, challenge: &[u8], key: &DeviceKey, role: u8, target: &str) {
    let mut signed = b"bubblepaw-relay-v1\0".to_vec();
    signed.extend_from_slice(challenge);
    signed.push(role);
    signed.extend_from_slice(target.as_bytes());
    let mut frame = vec![role];
    frame.extend_from_slice(&key.public_key());
    frame.extend_from_slice(&key.sign(&signed));
    frame.extend_from_slice(target.as_bytes());
    socket.send(Message::Binary(frame.into())).await.unwrap();
}

async fn opcode(socket: &mut Socket) -> u8 {
    let frame = tokio::time::timeout(Duration::from_secs(2), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    frame.into_data()[0]
}

#[tokio::test]
async fn pending_limit_releases_on_disconnect_and_registration_releases_on_shutdown() {
    let limits = Limits {
        pending_per_host: 1,
        ..Default::default()
    };
    let relay = start(RelayConfig {
        limits,
        ..Default::default()
    })
    .await;
    let url = format!("ws://{}", relay.local_addr());
    let host_key = key(4);
    let target = host_key.id().to_string();
    let (mut host, challenge) = socket(&url).await;
    auth(&mut host, &challenge, &host_key, 2, "").await;
    assert_eq!(opcode(&mut host).await, 4);
    let (mut first, challenge) = socket(&url).await;
    auth(&mut first, &challenge, &key(5), 3, &target).await;
    assert_eq!(opcode(&mut host).await, 5);
    let (mut rejected, challenge) = socket(&url).await;
    auth(&mut rejected, &challenge, &key(6), 3, &target).await;
    assert_eq!(opcode(&mut rejected).await, 10);
    first.close(None).await.unwrap();
    assert_eq!(opcode(&mut host).await, 8);
    let (mut next, challenge) = socket(&url).await;
    auth(&mut next, &challenge, &key(7), 3, &target).await;
    assert_eq!(opcode(&mut host).await, 5);
    relay.shutdown().await;
    assert!(
        tokio::time::timeout(Duration::from_secs(2), next.next())
            .await
            .unwrap()
            .is_none_or(|result| result.is_err() || matches!(result, Ok(Message::Close(_))))
    );
}

#[tokio::test]
async fn challenge_cannot_be_replayed_on_another_connection() {
    let relay = start(RelayConfig::default()).await;
    let url = format!("ws://{}", relay.local_addr());
    let (_first, old_challenge) = socket(&url).await;
    let (mut second, fresh_challenge) = socket(&url).await;
    assert_ne!(old_challenge, fresh_challenge);
    auth(&mut second, &old_challenge, &key(8), 2, "").await;
    assert!(
        tokio::time::timeout(Duration::from_secs(2), second.next())
            .await
            .unwrap()
            .is_none_or(|result| result.is_err() || matches!(result, Ok(Message::Close(_))))
    );
    relay.shutdown().await;
}

#[tokio::test]
async fn idle_and_oversized_connections_are_closed() {
    let limits = Limits {
        idle_timeout: Duration::from_millis(150),
        ..Default::default()
    };
    let relay = start(RelayConfig {
        limits: limits.clone(),
        ..Default::default()
    })
    .await;
    let url = format!("ws://{}", relay.local_addr());
    let (mut idle, _) = socket(&url).await;
    assert!(
        tokio::time::timeout(Duration::from_secs(2), idle.next())
            .await
            .unwrap()
            .is_none_or(|result| result.is_err() || matches!(result, Ok(Message::Close(_))))
    );
    let (mut oversized, challenge) = socket(&url).await;
    auth(&mut oversized, &challenge, &key(9), 2, "").await;
    assert_eq!(opcode(&mut oversized).await, 4);
    let mut frame = vec![7];
    frame.extend_from_slice(&1_u64.to_be_bytes());
    frame.resize(9 + limits.max_frame_bytes + 1, 42);
    oversized.send(Message::Binary(frame.into())).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), oversized.next())
            .await
            .unwrap()
            .is_none_or(|result| result.is_err() || matches!(result, Ok(Message::Close(_))))
    );
    relay.shutdown().await;
}

#[tokio::test]
async fn public_session_rejects_oversized_frames_before_queueing() {
    let relay = start(RelayConfig::default()).await;
    let url = format!("ws://{}", relay.local_addr());
    let host_key = key(10);
    let mut host = relay::register(&url, &host_key, Limits::default())
        .await
        .unwrap();
    let client_key = key(11);
    let host_id = host_key.id();
    let (accepted, connected) = tokio::join!(
        host.accept(),
        relay::connect(&url, &client_key, &host_id, Limits::default())
    );
    let mut accepted = accepted.unwrap();
    let connected = connected.unwrap();
    assert!(matches!(
        accepted.send_frame(&vec![0; 65_536]).await,
        Err(bubblepaw_remote::Error::OversizedFrame)
    ));
    drop(connected);
    drop(accepted);
    drop(host);
    relay.shutdown().await;
}
