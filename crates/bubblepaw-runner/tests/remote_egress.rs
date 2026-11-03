//! Model traffic through a paired device, with no external network: a fake
//! self-signed HTTPS provider on loopback, a forwarding host reached through an
//! in-process relay, and a requesting device whose local proxy carries the
//! TLS connection through the tunnel.
use anyhow::{Context, Result};
use async_trait::async_trait;
use bubblepaw_remote::relay::{Relay, RelayConfig};
use bubblepaw_runner::host_api::Profile;
use bubblepaw_runner::remote::egress::{
    EgressDial, EgressErrorKind, EgressHandle, EgressRoute, Refusal,
};
use bubblepaw_runner::remote::{
    DeviceKind, Limits, RemotePair, RemotePaths, RemoteReach, RemoteServeOptions, RemoteServer,
    SavedHost, list_hosts,
};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};

const PROVIDER_HOST: &str = "models.example.com";
const SECRET: &str = "prompt-marker-4f1c0b";

/// Records every byte the forwarding host's side of the provider socket sees.
struct Tap {
    inner: TcpStream,
    seen: Arc<Mutex<Vec<u8>>>,
}

impl AsyncRead for Tap {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        if poll.is_ready() {
            let seen = self.seen.clone();
            seen.lock()
                .unwrap()
                .extend_from_slice(&buf.filled()[before..]);
        }
        poll
    }
}

impl tokio::io::AsyncWrite for Tap {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, data)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// The fake provider: TLS (a certificate made for this run) over loopback,
/// answering any request with its path echoed back.
struct Provider {
    addr: SocketAddr,
    cert_pem: String,
    /// Bytes as the forwarder saw them on the wire (ciphertext).
    wire: Arc<Mutex<Vec<u8>>>,
    /// Requests as the provider decrypted them.
    requests: Arc<Mutex<Vec<String>>>,
}

async fn provider() -> Result<Provider> {
    let certified = rcgen::generate_simple_self_signed(vec![PROVIDER_HOST.to_owned()])?;
    let cert_pem = certified.cert.pem();
    let key = tokio_rustls::rustls::pki_types::PrivateKeyDer::try_from(
        certified.key_pair.serialize_der(),
    )
    .map_err(|error| anyhow::anyhow!("{error}"))?;
    let config = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certified.cert.der().clone()], key)?;
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let wire = Arc::new(Mutex::new(Vec::new()));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (tap, log) = (wire.clone(), requests.clone());
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            let (seen, log) = (tap.clone(), log.clone());
            tokio::spawn(async move {
                let tapped = Tap {
                    inner: socket,
                    seen,
                };
                let Ok(mut tls) = acceptor.accept(tapped).await else {
                    return;
                };
                let mut head = Vec::new();
                let mut chunk = [0u8; 1024];
                while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                    match tls.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => head.extend_from_slice(&chunk[..read]),
                    }
                }
                let text = String::from_utf8_lossy(&head).into_owned();
                let line = text.lines().next().unwrap_or_default().to_owned();
                log.lock().unwrap().push(line);
                let body = "provider says hello";
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = tls.write_all(reply.as_bytes()).await;
                let _ = tls.shutdown().await;
            });
        }
    });
    Ok(Provider {
        addr,
        cert_pem,
        wire,
        requests,
    })
}

/// Reaches the fake provider whatever name was asked for, and remembers the names.
struct TestDial {
    to: SocketAddr,
    asked: Mutex<Vec<String>>,
}

#[async_trait]
impl EgressDial for TestDial {
    async fn dial(&self, host: &str) -> Result<TcpStream, Refusal> {
        self.asked.lock().unwrap().push(host.to_owned());
        TcpStream::connect(self.to)
            .await
            .map_err(|error| Refusal::new("unreachable", error.to_string()))
    }
}

struct Fixture {
    _relay: Relay,
    root: tempfile::TempDir,
    forwarder: Option<RemoteServer>,
    dial: Arc<TestDial>,
    provider: Provider,
    paths: RemotePaths,
    saved: SavedHost,
}

/// A forwarding host and a requesting device paired with it; `egress` is the
/// grant the host user gives at pairing.
async fn fixture(egress: bool) -> Result<Fixture> {
    let relay = Relay::bind("127.0.0.1:0".parse()?, RelayConfig::default()).await?;
    let reach = RemoteReach::Relay(format!("ws://{}", relay.local_addr()));
    let root = tempfile::tempdir()?;
    let provider = provider().await?;
    let dial = Arc::new(TestDial {
        to: provider.addr,
        asked: Mutex::new(Vec::new()),
    });
    let options = RemoteServeOptions::in_data_dir(
        &root.path().join("forwarder"),
        reach,
        "Forwarding PC".into(),
    );
    let server = RemoteServer::start_with_dial(options, None, dial.clone())?;
    server.wait_online(Duration::from_secs(10)).await?;
    let address = server.address().context("an online host has an address")?;
    let paths = RemotePaths::in_data_dir(&root.path().join("requester"));
    let mut offer = server.pair_with_egress(Profile::Read, egress).await?;
    let pairing = bubblepaw_runner::remote::pair(RemotePair {
        address: address.clone(),
        host_id: server.host_id(),
        code: offer.code().to_owned(),
        key_file: paths.key_file.clone(),
        hosts: paths.hosts.clone(),
        device_name: "Requesting PC".into(),
        device_kind: DeviceKind::Desktop,
        limits: Limits::default(),
    });
    let (pending, request) = tokio::join!(pairing, offer.next());
    let (pending, request) = (pending?, request.context("a pairing request")?);
    let (paired, stored) = tokio::join!(pending.finish(), request.approve(None));
    let paired = paired?;
    let stored = stored?;
    assert_eq!(stored.egress, egress, "the device list shows the grant");
    assert_eq!(paired.egress, egress, "the device learns the grant");
    let saved = list_hosts(&paths.hosts)?
        .into_iter()
        .find(|host| host.id == server.host_id())
        .context("the requester saved the host")?;
    assert_eq!(saved.egress, egress);
    Ok(Fixture {
        _relay: relay,
        root,
        forwarder: Some(server),
        dial,
        provider,
        paths,
        saved,
    })
}

impl Fixture {
    fn handle(&self, declared: &[&str]) -> Result<Arc<EgressHandle>> {
        let route = EgressRoute::new(
            &self.paths,
            &self.saved.id,
            declared.iter().map(|host| (*host).to_owned()).collect(),
        )?;
        Ok(EgressHandle::new(route))
    }

    /// An HTTPS client that trusts the fake provider's certificate and goes
    /// through the handle's proxy, as a model adapter's client does.
    fn client(&self, handle: &EgressHandle) -> Result<reqwest::Client> {
        Ok(reqwest::Client::builder()
            .no_proxy()
            .proxy(reqwest::Proxy::all(handle.proxy_url())?)
            .add_root_certificate(reqwest::Certificate::from_pem(
                self.provider.cert_pem.as_bytes(),
            )?)
            .timeout(Duration::from_secs(20))
            .build()?)
    }

    fn asked(&self) -> Vec<String> {
        self.dial.asked.lock().unwrap().clone()
    }
}

/// One raw CONNECT to the handle's proxy; the status line it answers with.
async fn connect_status(handle: &EgressHandle, authority: &str) -> Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", handle.proxy()?.port())).await?;
    stream
        .write_all(format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes())
        .await?;
    let mut reply = String::new();
    let mut chunk = [0u8; 512];
    let read = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let read = stream.read(&mut chunk).await?;
            reply.push_str(&String::from_utf8_lossy(&chunk[..read]));
            if read == 0 || reply.contains("\r\n") {
                return std::io::Result::Ok(());
            }
        }
    })
    .await?;
    read?;
    Ok(reply.lines().next().unwrap_or_default().to_owned())
}

#[tokio::test]
async fn tls_reaches_the_provider_and_the_forwarder_sees_only_ciphertext() -> Result<()> {
    let fixture = fixture(true).await?;
    let handle = fixture.handle(&[PROVIDER_HOST])?;
    let client = fixture.client(&handle)?;
    let response = client
        .get(format!("https://{PROVIDER_HOST}/v1/{SECRET}"))
        .header("authorization", format!("Bearer {SECRET}"))
        .send()
        .await
        .context("request through the paired device")?;
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await?, "provider says hello");
    // The provider decrypted the request; the forwarder never could.
    let requests = fixture.provider.requests.lock().unwrap().clone();
    assert_eq!(requests, vec![format!("GET /v1/{SECRET} HTTP/1.1")]);
    assert_eq!(fixture.asked(), vec![PROVIDER_HOST.to_owned()]);
    let wire = fixture.provider.wire.lock().unwrap().clone();
    assert!(wire.len() > 100, "the forwarder carried a TLS exchange");
    assert_eq!(wire[0], 0x16, "the first bytes are a TLS handshake record");
    let needle = SECRET.as_bytes();
    assert!(
        !wire.windows(needle.len()).any(|window| window == needle),
        "the forwarder's side of the connection holds no plaintext"
    );
    assert!(handle.failure().is_none());
    Ok(())
}

#[tokio::test]
async fn a_device_without_the_grant_is_refused_by_name() -> Result<()> {
    let fixture = fixture(false).await?;
    let handle = fixture.handle(&[PROVIDER_HOST])?;
    let client = fixture.client(&handle)?;
    let error = client
        .get(format!("https://{PROVIDER_HOST}/v1/models"))
        .send()
        .await
        .expect_err("a device without the grant gets no tunnel");
    drop(error);
    let failure = handle.failure().context("the refusal is recorded")?;
    match &failure.kind {
        EgressErrorKind::Refused { code, .. } => assert_eq!(code, "not_granted"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(failure.is_permanent());
    let text = failure.to_string();
    assert!(text.contains("Forwarding PC"), "{text}");
    assert!(text.contains("refused"), "{text}");
    assert!(fixture.asked().is_empty(), "nothing was dialed");
    assert!(fixture.provider.requests.lock().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn the_proxy_and_forwarder_are_not_an_open_proxy() -> Result<()> {
    let fixture = fixture(true).await?;
    let handle = fixture.handle(&[PROVIDER_HOST])?;
    for authority in [
        "other.example.com:443",
        "127.0.0.1:443",
        "10.0.0.5:443",
        "192.168.1.1:443",
        "169.254.169.254:443",
        "100.64.0.1:443",
        "[::1]:443",
        "localhost:443",
        "printer.local:443",
        "models.example.com:80",
        "models.example.com:22",
    ] {
        let status = connect_status(&handle, authority).await?;
        assert!(status.contains("403"), "{authority} got {status}");
    }
    // Nothing but CONNECT is served.
    let mut stream = TcpStream::connect(("127.0.0.1", handle.proxy()?.port())).await?;
    stream
        .write_all(b"GET http://models.example.com/ HTTP/1.1\r\nHost: models.example.com\r\n\r\n")
        .await?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).await?;
    assert!(reply.starts_with("HTTP/1.1 405"), "{reply}");
    assert!(fixture.asked().is_empty(), "nothing was dialed");
    // The declared host itself still works.
    assert!(
        connect_status(&handle, "models.example.com:443")
            .await?
            .contains("200")
    );
    Ok(())
}

#[tokio::test]
async fn an_offline_forwarder_is_named_and_there_is_no_direct_fallback() -> Result<()> {
    let mut fixture = fixture(true).await?;
    let handle = fixture.handle(&[PROVIDER_HOST])?;
    fixture
        .forwarder
        .take()
        .context("the forwarder")?
        .shutdown()
        .await;
    let client = fixture.client(&handle)?;
    client
        .get(format!("https://{PROVIDER_HOST}/v1/models"))
        .send()
        .await
        .expect_err("an offline forwarder carries nothing");
    let failure = handle.failure().context("the failure is recorded")?;
    assert!(
        matches!(failure.kind, EgressErrorKind::Offline(_)),
        "{failure:?}"
    );
    assert!(!failure.is_permanent());
    let text = failure.to_string();
    assert!(text.contains("Forwarding PC"), "{text}");
    assert!(text.contains("offline"), "{text}");
    assert!(fixture.provider.requests.lock().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn a_device_that_is_not_paired_is_named() -> Result<()> {
    let fixture = fixture(true).await?;
    let stranger = bubblepaw_remote::device::DeviceKey::generate()?
        .id()
        .to_string();
    let route = EgressRoute::new(&fixture.paths, &stranger, vec![PROVIDER_HOST.to_owned()])?;
    let handle = EgressHandle::new(route);
    let status = connect_status(&handle, "models.example.com:443").await?;
    assert!(status.contains("403"), "{status}");
    let failure = handle.failure().context("the failure is recorded")?;
    assert_eq!(failure.kind, EgressErrorKind::NotPaired);
    assert!(failure.to_string().contains(&stranger));
    let _ = &fixture.root;
    Ok(())
}

fn settings(root: &std::path::Path, provider: &str) -> Result<bubblepaw_runner::config::Settings> {
    let file = root.join("config.toml");
    std::fs::write(&file, format!("[providers.p]\n{provider}\n"))?;
    bubblepaw_runner::config::EffectiveConfig::load(bubblepaw_runner::config::LoadOptions {
        workspace: root.to_owned(),
        home: None,
        custom_file: Some(file),
        env: Vec::new(),
        cli: Vec::new(),
    })?
    .settings()
}

#[test]
fn provider_via_is_validated_and_makes_a_route() -> Result<()> {
    let root = tempfile::tempdir()?;
    let id = bubblepaw_remote::device::DeviceKey::generate()?
        .id()
        .to_string();
    let base =
        "kind='chat-completions'\nbase_url='https://api.example.com/v1'\nkey_env='NOT_A_REAL_KEY'";
    let direct = settings(root.path(), base)?;
    assert!(direct.egress_for("p")?.is_none());
    let via = settings(root.path(), &format!("{base}\nvia='{id}'"))?;
    let handle = via.egress_for("p")?.context("via makes a handle")?;
    assert_eq!(handle.route().device, id);
    assert_eq!(handle.route().declared, vec!["api.example.com".to_owned()]);
    for (bad, why) in [
        (format!("{base}\nvia='not-a-device'"), "device ID"),
        (
            format!("kind='chat-completions'\nbase_url='http://api.example.com/v1'\nvia='{id}'"),
            "https",
        ),
        (
            format!("kind='chat-completions'\nbase_url='https://10.0.0.5/v1'\nvia='{id}'"),
            "via",
        ),
        (format!("kind='scripted'\nvia='{id}'"), "applies to"),
    ] {
        let error = settings(root.path(), &bad).err().context(why)?;
        assert!(format!("{error:#}").contains(why), "{why}: {error:#}");
    }
    Ok(())
}
