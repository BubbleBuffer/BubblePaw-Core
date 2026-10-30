use std::time::Duration;

use bubblepaw_remote::{
    Error,
    device::{DeviceKey, noise_public_key},
    frame::{ByteFrames, FrameIo, MAX_FRAME_LEN, WebSocketFrames, websocket_config},
    tunnel::{self, MAX_CHUNK_PAYLOAD, MAX_MESSAGE_LEN, Side, Tunnel},
};
use tokio::{
    io::{AsyncWriteExt, DuplexStream, duplex},
    net::{TcpListener, TcpStream},
};

async fn paired_bytes() -> (
    Tunnel<ByteFrames<DuplexStream>>,
    Tunnel<ByteFrames<DuplexStream>>,
) {
    let alice = DeviceKey::generate().unwrap();
    let bob = DeviceKey::generate().unwrap();
    let (left, right) = duplex(1024);
    let alice_trust = [bob.public_key()];
    let bob_trust = [alice.public_key()];
    let (left, right) = tokio::join!(
        tunnel::connect(ByteFrames::new(left), &alice, &alice_trust),
        tunnel::accept(ByteFrames::new(right), &bob, &bob_trust),
    );
    let left = left.expect("paired initiator");
    let right = right.expect("paired responder");
    assert_eq!(
        left.peer_static(),
        &noise_public_key(&bob.public_key()).unwrap()
    );
    assert_eq!(
        right.peer_static(),
        &noise_public_key(&alice.public_key()).unwrap()
    );
    assert_eq!(left.handshake_hash(), right.handshake_hash());
    (left, right)
}

#[tokio::test]
async fn duplex_round_trip_spans_noise_records_and_preserves_messages() {
    let (mut alice, mut bob) = paired_bytes().await;
    for length in [0, 1, MAX_CHUNK_PAYLOAD, MAX_CHUNK_PAYLOAD + 1, 200_000] {
        let payload: Vec<_> = (0..length).map(|index| (index % 251) as u8).collect();
        let (sent, received) = tokio::join!(alice.send(&payload), bob.recv());
        sent.unwrap();
        assert_eq!(received.unwrap(), payload);
        let (sent, received) = tokio::join!(bob.send(b"response"), alice.recv());
        sent.unwrap();
        assert_eq!(received.unwrap(), b"response");
    }
}

#[tokio::test]
async fn whole_message_limit_accepts_boundary_and_rejects_larger_without_poisoning() {
    let (mut alice, mut bob) = paired_bytes().await;
    let payload = vec![42; MAX_MESSAGE_LEN];
    let (sent, received) = tokio::join!(alice.send(&payload), bob.recv());
    sent.unwrap();
    assert_eq!(received.unwrap(), payload);
    assert!(matches!(
        alice.send(&vec![0; MAX_MESSAGE_LEN + 1]).await,
        Err(Error::OversizedFrame)
    ));
    let (sent, received) = tokio::join!(alice.send(b"still usable"), bob.recv());
    sent.unwrap();
    assert_eq!(received.unwrap(), b"still usable");
}

#[tokio::test]
async fn rejection_is_mutual_even_when_only_one_side_distrusts() {
    for reject_initiator in [false, true] {
        let alice = DeviceKey::generate().unwrap();
        let bob = DeviceKey::generate().unwrap();
        let (left, right) = duplex(1024);
        let alice_trust = if reject_initiator {
            vec![]
        } else {
            vec![bob.public_key()]
        };
        let bob_trust = if reject_initiator {
            vec![alice.public_key()]
        } else {
            vec![]
        };
        let (left, right) = tokio::join!(
            tunnel::connect(ByteFrames::new(left), &alice, &alice_trust),
            tunnel::accept(ByteFrames::new(right), &bob, &bob_trust),
        );
        assert!(matches!(left, Err(Error::UnpairedPeer)));
        assert!(matches!(right, Err(Error::UnpairedPeer)));
    }
}

#[tokio::test]
async fn admission_completes_over_a_one_byte_duplex_buffer() {
    let alice = DeviceKey::generate().unwrap();
    let bob = DeviceKey::generate().unwrap();
    let (left, right) = duplex(1);
    let alice_trust = [bob.public_key()];
    let bob_trust = [alice.public_key()];
    let handshake = async {
        let (left, right) = tokio::join!(
            tunnel::connect(ByteFrames::new(left), &alice, &alice_trust),
            tunnel::accept(ByteFrames::new(right), &bob, &bob_trust),
        );
        assert!(left.is_ok());
        assert!(right.is_ok());
    };
    tokio::time::timeout(Duration::from_secs(5), handshake)
        .await
        .unwrap();
}

#[tokio::test]
async fn raw_psk_handshake_agrees_on_peer_and_hash_without_admission() {
    let alice = DeviceKey::generate().unwrap();
    let bob = DeviceKey::generate().unwrap();
    let (left, right) = duplex(1024);
    let mut left = ByteFrames::new(left);
    let mut right = ByteFrames::new(right);
    let psk = [7; 32];
    let (initiator, responder) = tokio::join!(
        tunnel::exchange(&mut left, &alice, Side::Initiator, Some(&psk)),
        tunnel::exchange(&mut right, &bob, Side::Responder, Some(&psk)),
    );
    let initiator = initiator.unwrap();
    let responder = responder.unwrap();
    assert_eq!(initiator.handshake_hash, responder.handshake_hash);
    let mut left = Tunnel::from_handshake(left, initiator);
    let mut right = Tunnel::from_handshake(right, responder);
    assert!(left.authorization().is_none());
    assert!(right.authorization().is_none());
    let (sent, received) = tokio::join!(left.send(b"pairing confirmed"), right.recv());
    sent.unwrap();
    assert_eq!(received.unwrap(), b"pairing confirmed");
}

#[tokio::test]
async fn raw_psk_handshake_rejects_different_codes_at_responder() {
    let alice = DeviceKey::generate().unwrap();
    let bob = DeviceKey::generate().unwrap();
    let (left, right) = duplex(1024);
    let mut left = ByteFrames::new(left);
    let mut right = ByteFrames::new(right);
    let (initiator, responder) = tokio::join!(
        tunnel::exchange(&mut left, &alice, Side::Initiator, Some(&[1; 32])),
        tunnel::exchange(&mut right, &bob, Side::Responder, Some(&[2; 32])),
    );
    // XXpsk3 initiators finish before the responder validates the final PSK.
    assert!(initiator.is_ok());
    assert!(matches!(responder, Err(Error::Noise(_))));
}

#[tokio::test]
async fn byte_frames_reject_oversize_prefix_before_reading_payload() {
    let (mut left, right) = duplex(32);
    left.write_u32((MAX_FRAME_LEN + 1) as u32).await.unwrap();
    let mut receiver = ByteFrames::new(right);
    assert!(matches!(
        receiver.recv_frame().await,
        Err(Error::OversizedFrame)
    ));
    let mut sender = ByteFrames::new(left);
    assert!(matches!(
        sender.send_frame(&vec![0; MAX_FRAME_LEN + 1]).await,
        Err(Error::OversizedFrame)
    ));
}

#[tokio::test]
async fn cancelled_receive_poisoning_prevents_nonce_or_framing_reuse() {
    let (_alice, mut bob) = paired_bytes().await;
    let result = tokio::time::timeout(Duration::from_millis(10), bob.recv()).await;
    assert!(result.is_err());
    assert!(matches!(bob.recv().await, Err(Error::Protocol(_))));
    assert!(matches!(
        bob.send(b"cannot reuse").await,
        Err(Error::Protocol(_))
    ));
}

#[tokio::test]
async fn invalid_authenticated_chunk_marker_invalidates_tunnel() {
    let alice = DeviceKey::generate().unwrap();
    let bob = DeviceKey::generate().unwrap();
    let (left, right) = duplex(1024);
    let mut left = ByteFrames::new(left);
    let mut right = ByteFrames::new(right);
    let (initiator, responder) = tokio::join!(
        tunnel::exchange(&mut left, &alice, Side::Initiator, None),
        tunnel::exchange(&mut right, &bob, Side::Responder, None),
    );
    let mut sender = initiator.unwrap().transport;
    let mut receiver = Tunnel::from_handshake(right, responder.unwrap());
    let mut ciphertext = vec![0; MAX_FRAME_LEN];
    let length = sender.write_message(&[2, 42], &mut ciphertext).unwrap();
    left.send_frame(&ciphertext[..length]).await.unwrap();
    assert!(matches!(receiver.recv().await, Err(Error::Protocol(_))));
    assert!(matches!(receiver.recv().await, Err(Error::Protocol(_))));
}

#[tokio::test]
async fn modified_ciphertext_and_replayed_records_fail_authentication() {
    for tamper in [false, true] {
        let alice = DeviceKey::generate().unwrap();
        let bob = DeviceKey::generate().unwrap();
        let (left, right) = duplex(1024);
        let mut left = ByteFrames::new(left);
        let mut right = ByteFrames::new(right);
        let (initiator, responder) = tokio::join!(
            tunnel::exchange(&mut left, &alice, Side::Initiator, None),
            tunnel::exchange(&mut right, &bob, Side::Responder, None),
        );
        let mut sender = initiator.unwrap().transport;
        let mut receiver = Tunnel::from_handshake(right, responder.unwrap());
        let mut ciphertext = vec![0; MAX_FRAME_LEN];
        let length = sender.write_message(&[0, 42], &mut ciphertext).unwrap();
        if tamper {
            ciphertext[0] ^= 1;
        } else {
            left.send_frame(&ciphertext[..length]).await.unwrap();
            assert_eq!(receiver.recv().await.unwrap(), [42]);
        }
        left.send_frame(&ciphertext[..length]).await.unwrap();
        assert!(matches!(receiver.recv().await, Err(Error::Noise(_))));
        assert!(matches!(receiver.recv().await, Err(Error::Protocol(_))));
    }
}

#[tokio::test]
async fn receiver_rejects_assembled_messages_over_limit() {
    let alice = DeviceKey::generate().unwrap();
    let bob = DeviceKey::generate().unwrap();
    let (left, right) = duplex(1024);
    let mut left = ByteFrames::new(left);
    let mut right = ByteFrames::new(right);
    let (initiator, responder) = tokio::join!(
        tunnel::exchange(&mut left, &alice, Side::Initiator, None),
        tunnel::exchange(&mut right, &bob, Side::Responder, None),
    );
    let mut sender = initiator.unwrap().transport;
    let receiver = async move {
        Tunnel::from_handshake(right, responder.unwrap())
            .recv()
            .await
    };
    let malicious_sender = async move {
        let mut plaintext = vec![1; MAX_CHUNK_PAYLOAD + 1];
        plaintext[0] = 1;
        let mut ciphertext = vec![0; MAX_FRAME_LEN];
        for _ in 0..=(MAX_MESSAGE_LEN / MAX_CHUNK_PAYLOAD) {
            let length = sender.write_message(&plaintext, &mut ciphertext).unwrap();
            if left.send_frame(&ciphertext[..length]).await.is_err() {
                break;
            }
        }
    };
    let (_, received) = tokio::join!(malicious_sender, receiver);
    assert!(matches!(received, Err(Error::OversizedFrame)));
}

#[tokio::test]
async fn real_tcp_websocket_handshake_and_bidirectional_large_payload() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let alice = DeviceKey::generate().unwrap();
    let bob = DeviceKey::generate().unwrap();
    let payload = vec![93; 200_000];
    let server = async {
        let (stream, _) = listener.accept().await.unwrap();
        let socket = tokio_tungstenite::accept_async_with_config(stream, Some(websocket_config()))
            .await
            .unwrap();
        let mut tunnel = tunnel::accept(WebSocketFrames::new(socket), &bob, &[alice.public_key()])
            .await
            .unwrap();
        let received = tunnel.recv().await.unwrap();
        assert_eq!(received, payload);
        tunnel.send(&received).await.unwrap();
    };
    let client = async {
        let stream = TcpStream::connect(address).await.unwrap();
        let (socket, _) = tokio_tungstenite::client_async_with_config(
            format!("ws://{address}/tunnel"),
            stream,
            Some(websocket_config()),
        )
        .await
        .unwrap();
        let mut tunnel = tunnel::connect(WebSocketFrames::new(socket), &alice, &[bob.public_key()])
            .await
            .unwrap();
        tunnel.send(&payload).await.unwrap();
        assert_eq!(tunnel.recv().await.unwrap(), payload);
    };
    tokio::join!(server, client);
}

#[tokio::test]
async fn websocket_requires_binary_and_preallocation_bounds() {
    use futures_util::SinkExt;
    use tokio_tungstenite::{
        WebSocketStream,
        tungstenite::{Message, protocol::Role},
    };
    let (left, right) = duplex(1024);
    let mut left =
        WebSocketStream::from_raw_socket(left, Role::Client, Some(websocket_config())).await;
    let right =
        WebSocketStream::from_raw_socket(right, Role::Server, Some(websocket_config())).await;
    left.send(Message::Text("plaintext is not a Noise frame".into()))
        .await
        .unwrap();
    assert!(matches!(
        WebSocketFrames::new(right).recv_frame().await,
        Err(Error::Protocol(_))
    ));
    let (_, right) = duplex(1024);
    let right = WebSocketStream::from_raw_socket(right, Role::Server, None).await;
    assert!(matches!(
        WebSocketFrames::new(right).recv_frame().await,
        Err(Error::Protocol(_))
    ));
}
