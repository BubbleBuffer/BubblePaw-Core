use std::time::{Duration, Instant};

use bubblepaw_remote::{
    Error,
    device::DeviceKey,
    frame::ByteFrames,
    pairing::{self, ClientOptions, PairingCode, PendingClient, PendingHost},
    store::{PairStore, PairedClient, PairedHost, Profile},
};
use tokio::io::DuplexStream;

type Host = PendingHost<ByteFrames<DuplexStream>>;
type Client = PendingClient<ByteFrames<DuplexStream>>;

fn keys() -> (DeviceKey, DeviceKey) {
    (
        DeviceKey::from_seed(&[1; 32]),
        DeviceKey::from_seed(&[2; 32]),
    )
}

fn options(host: &DeviceKey) -> ClientOptions {
    ClientOptions {
        host_id: host.id(),
        reach_address: "ws://relay.example.test/".to_owned(),
        client_name: "Phone".to_owned(),
    }
}

async fn start(
    ticket: &mut PairingCode,
    supplied_code: &str,
) -> (
    bubblepaw_remote::Result<Host>,
    bubblepaw_remote::Result<Client>,
) {
    let (host, client) = keys();
    let (host_io, client_io) = tokio::io::duplex(4096);
    tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(
            pairing::accept(ByteFrames::new(host_io), &host, ticket, "Workstation"),
            pairing::connect(
                ByteFrames::new(client_io),
                &client,
                supplied_code,
                options(&host)
            ),
        )
    })
    .await
    .expect("pairing must finish or fail promptly")
}

#[tokio::test]
async fn success_comparison_numbers_match_and_both_stores_persist() {
    let mut ticket = PairingCode::generate().unwrap();
    let code = ticket.code().to_owned();
    assert_eq!(code.len(), 10);
    assert!(!format!("{ticket:?}").contains(&code));
    let (host, client) = start(&mut ticket, &code).await;
    let (host, client) = (host.unwrap(), client.unwrap());
    assert!(ticket.used());
    assert_eq!(host.comparison(), client.comparison());
    assert_eq!(host.comparison().len(), 6);
    assert!(host.comparison().bytes().all(|b| b.is_ascii_digit()));
    assert_eq!(host.client_suggested_name(), "Phone");
    let dir = tempfile::tempdir().unwrap();
    let host_path = dir.path().join("host/clients.json");
    let client_path = dir.path().join("client/hosts.json");
    let mut clients = PairStore::<PairedClient>::open(&host_path).unwrap();
    let mut hosts = PairStore::<PairedHost>::open(&client_path).unwrap();
    assert_eq!(clients.devices().count(), 0);
    assert_eq!(hosts.devices().count(), 0);
    let (host, client) = tokio::join!(
        host.approve("My phone".to_owned(), Profile::Edit, &mut clients),
        client.finish(&mut hosts),
    );
    let (mut host, mut client) = (host.unwrap(), client.unwrap());
    assert_eq!(host.authorization().unwrap().profile, Profile::Edit);
    assert_eq!(client.authorization().unwrap().profile, Profile::Full);
    let (send, receive) = tokio::join!(client.send(b"paired protocol message"), host.recv());
    send.unwrap();
    assert_eq!(receive.unwrap(), b"paired protocol message");
    let (host_key, client_key) = keys();
    let clients = PairStore::<PairedClient>::open(host_path).unwrap();
    let mut hosts = PairStore::<PairedHost>::open(client_path).unwrap();
    let paired_client = clients.get(&client_key.id()).unwrap();
    assert_eq!(paired_client.name, "My phone");
    assert_eq!(paired_client.profile, Profile::Edit);
    let paired_host = hosts.get(&host_key.id()).unwrap();
    assert_eq!(paired_host.public_key, host_key.public_key());
    assert_eq!(paired_host.name, "Workstation");
    assert_eq!(paired_host.reach_address, "ws://relay.example.test/");
    assert!(hosts.revoke(&host_key.id()).unwrap());
    assert!(!hosts.revoke(&host_key.id()).unwrap());
}

#[tokio::test]
async fn wrong_code_fails_distinctly_on_both_sides() {
    let mut ticket = PairingCode::generate().unwrap();
    let mut wrong = ticket.code().as_bytes().to_vec();
    wrong[0] = if wrong[0] == b'0' { b'1' } else { b'0' };
    let wrong = String::from_utf8(wrong).unwrap();
    let (host, client) = start(&mut ticket, &wrong).await;
    assert!(matches!(host, Err(Error::WrongCode)));
    assert!(matches!(client, Err(Error::WrongCode)));
    assert!(!ticket.used());
}

#[tokio::test]
async fn expired_code_fails_distinctly_on_both_sides() {
    let mut ticket = PairingCode::generate_at(Instant::now() - Duration::from_secs(301)).unwrap();
    let code = ticket.code().to_owned();
    let (host, client) = start(&mut ticket, &code).await;
    assert!(matches!(host, Err(Error::ExpiredCode)));
    assert!(matches!(client, Err(Error::ExpiredCode)));
}

#[tokio::test]
async fn reused_code_fails_distinctly_on_both_sides() {
    let mut ticket = PairingCode::generate().unwrap();
    let code = ticket.code().to_owned();
    let (host, client) = start(&mut ticket, &code).await;
    drop(host.unwrap());
    drop(client.unwrap());
    let (host, client) = start(&mut ticket, &code).await;
    assert!(matches!(host, Err(Error::ReusedCode)));
    assert!(matches!(client, Err(Error::ReusedCode)));
}

#[tokio::test]
async fn declined_comparison_never_saves_either_peer() {
    let mut ticket = PairingCode::generate().unwrap();
    let code = ticket.code().to_owned();
    let (host, client) = start(&mut ticket, &code).await;
    let dir = tempfile::tempdir().unwrap();
    let mut hosts = PairStore::<PairedHost>::open(dir.path().join("hosts.json")).unwrap();
    let (host, client) = tokio::join!(host.unwrap().decline(), client.unwrap().finish(&mut hosts));
    assert!(matches!(host, Err(Error::ComparisonDeclined)));
    assert!(matches!(client, Err(Error::ComparisonDeclined)));
    assert_eq!(hosts.devices().count(), 0);
    assert!(!dir.path().join("hosts.json").exists());
    assert!(ticket.used());
}

struct StalledFrames;

#[async_trait::async_trait]
impl bubblepaw_remote::frame::FrameIo for StalledFrames {
    async fn send_frame(&mut self, _: &[u8]) -> bubblepaw_remote::Result<()> {
        std::future::pending().await
    }
    async fn recv_frame(&mut self) -> bubblepaw_remote::Result<Vec<u8>> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn stalled_hello_cannot_keep_pairing_alive_beyond_its_deadline() {
    let (host, _) = keys();
    let mut ticket =
        PairingCode::generate_at(Instant::now() - Duration::from_millis(299_950)).unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        pairing::accept(StalledFrames, &host, &mut ticket, "Desktop"),
    )
    .await
    .expect("stalled hello must expire promptly");
    assert!(matches!(result, Err(Error::ExpiredCode)));
}

#[tokio::test]
async fn expired_status_diagnostic_cannot_hang() {
    let (host, _) = keys();
    let mut ticket = PairingCode::generate_at(Instant::now() - Duration::from_secs(301)).unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        pairing::accept(StalledFrames, &host, &mut ticket, "Desktop"),
    )
    .await
    .expect("diagnostics must not hang on a non-reading peer");
    assert!(matches!(result, Err(Error::ExpiredCode)));
}
