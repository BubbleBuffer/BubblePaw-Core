use bubblepaw_remote::{
    device::DeviceKey,
    frame::FrameIo,
    relay::{self, Limits, Relay, RelayConfig},
    tunnel,
};
use futures_util::StreamExt;
use std::time::Duration;

#[tokio::test]
async fn per_ip_connection_cap_is_enforced_and_released_after_disconnect() {
    let relay = Relay::bind(
        "127.0.0.1:0".parse().unwrap(),
        RelayConfig {
            limits: Limits {
                connections_per_ip: 1,
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let url = format!("ws://{}", relay.local_addr());
    let (mut first, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    first.next().await.unwrap().unwrap(); // Challenge proves the first slot is held.
    assert!(tokio_tungstenite::connect_async(&url).await.is_err());
    first.close(None).await.unwrap();
    let (mut replacement, _) = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(connection) = tokio_tungstenite::connect_async(&url).await {
                break connection;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    replacement.next().await.unwrap().unwrap();
    relay.shutdown().await;
}

#[tokio::test]
async fn stale_data_after_client_disconnect_preserves_another_live_tunnel() {
    let relay = Relay::bind("127.0.0.1:0".parse().unwrap(), RelayConfig::default())
        .await
        .unwrap();
    let url = format!("ws://{}", relay.local_addr());
    let host_key = DeviceKey::from_seed(&[31; 32]);
    let departed_key = DeviceKey::from_seed(&[32; 32]);
    let live_key = DeviceKey::from_seed(&[33; 32]);
    let target = host_key.id();
    let mut host = relay::register(&url, &host_key, Limits::default())
        .await
        .unwrap();
    let (departed_host, departed_client) = tokio::join!(
        host.accept(),
        relay::connect(&url, &departed_key, &target, Limits::default())
    );
    let mut departed_host = departed_host.unwrap();
    let departed_client = departed_client.unwrap();
    let (live_host, live_client) = tokio::join!(
        host.accept(),
        relay::connect(&url, &live_key, &target, Limits::default())
    );
    let host_trusted = [live_key.public_key()];
    let client_trusted = [host_key.public_key()];
    let (live_host, live_client) = tokio::join!(
        tunnel::accept(live_host.unwrap(), &host_key, &host_trusted),
        tunnel::connect(live_client.unwrap(), &live_key, &client_trusted)
    );
    let mut live_host = live_host.unwrap();
    let mut live_client = live_client.unwrap();
    drop(departed_client);
    assert!(
        tokio::time::timeout(Duration::from_secs(2), departed_host.recv_frame())
            .await
            .unwrap()
            .is_err()
    );
    // A caller may have prepared this write before learning that its client left.
    departed_host
        .send_frame(b"stale encrypted record")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        let client = async {
            live_client.send(b"still connected").await.unwrap();
            assert_eq!(live_client.recv().await.unwrap(), b"still connected");
        };
        let server = async {
            let message = live_host.recv().await.unwrap();
            live_host.send(&message).await.unwrap();
        };
        tokio::join!(client, server);
    })
    .await
    .unwrap();
    drop(live_host);
    drop(live_client);
    drop(departed_host);
    drop(host);
    relay.shutdown().await;
}
