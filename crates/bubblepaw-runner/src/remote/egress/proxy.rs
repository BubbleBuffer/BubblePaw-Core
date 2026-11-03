//! The local proxy: a tiny HTTP CONNECT proxy on 127.0.0.1 that the model HTTP
//! client uses. Each CONNECT becomes one egress session to the paired
//! forwarding device, and the client's TLS runs through it end to end. It
//! refuses everything but `CONNECT <declared host>:443`, so nothing else on
//! this machine can use it to reach anywhere else.
use super::allow::{self, EGRESS_PORT};
use super::pump::pump;
use super::route::{EgressError, EgressErrorKind, EgressRoute};
use bubblepaw_core::CancellationToken;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// How long a client may take to send its CONNECT line.
const HEAD_WAIT: Duration = Duration::from_secs(10);
/// Longest request head accepted.
const HEAD_LIMIT: usize = 8 * 1024;
/// Most connections served at once.
const MAX_CONNECTIONS: usize = 64;

/// A running proxy. Dropping it stops the listener and every connection.
pub struct EgressProxy {
    port: u16,
    stop: CancellationToken,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl EgressProxy {
    /// Bind 127.0.0.1 on a random port and serve `route` on a thread of its own
    /// (so it works from any caller, inside a Tokio runtime or not).
    /// `failure` receives the latest connection failure and is cleared by the
    /// next success.
    pub(super) fn start(
        route: EgressRoute,
        failure: Arc<Mutex<Option<EgressError>>>,
    ) -> std::io::Result<Self> {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let stop = CancellationToken::new();
        let serve_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("bubblepaw-egress-proxy".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                runtime.block_on(async move {
                    if let Ok(listener) = TcpListener::from_std(listener) {
                        serve(listener, Arc::new(route), failure, serve_stop).await;
                    }
                });
            })?;
        Ok(Self {
            port,
            stop,
            thread: Mutex::new(Some(thread)),
        })
    }

    /// The proxy's address for the HTTP client, `http://127.0.0.1:<port>`.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Stop serving and wait for the proxy's thread.
    pub fn shutdown(&self) {
        self.stop.cancel();
        let thread = self.thread.lock().ok().and_then(|mut thread| thread.take());
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
}

impl Drop for EgressProxy {
    fn drop(&mut self) {
        // Do not join here: a drop on an async worker must not block.
        self.stop.cancel();
    }
}

async fn serve(
    listener: TcpListener,
    route: Arc<EgressRoute>,
    failure: Arc<Mutex<Option<EgressError>>>,
    stop: CancellationToken,
) {
    let limit = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        let accepted = tokio::select! {
            _ = stop.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        let Ok((stream, peer)) = accepted else {
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_millis(50)) => continue,
            }
        };
        // Only this machine, whatever the listener happened to bind.
        if !peer.ip().is_loopback() {
            continue;
        }
        let Ok(permit) = limit.clone().try_acquire_owned() else {
            let _ = refuse(stream, "503 Service Unavailable", "too many connections").await;
            continue;
        };
        let (route, failure, stop) = (route.clone(), failure.clone(), stop.clone());
        tokio::spawn(async move {
            connection(stream, peer, &route, &failure, stop).await;
            drop(permit);
        });
    }
}

async fn refuse(mut stream: TcpStream, status: &str, message: &str) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{message}",
        message.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

/// Read the request head: everything up to the blank line, and nothing after
/// it (a client waits for the 200 before it sends the TLS hello).
async fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        head.extend_from_slice(&chunk[..read]);
        if let Some(end) = head.windows(4).position(|window| window == b"\r\n\r\n") {
            return (end + 4 == head.len())
                .then(|| String::from_utf8(head).ok())
                .flatten();
        }
        if head.len() > HEAD_LIMIT {
            return None;
        }
    }
}

/// `CONNECT host:port HTTP/1.x` to `(host, port)`.
fn parse_connect(head: &str) -> Option<(String, u16)> {
    let line = head.lines().next()?;
    let mut parts = line.split_ascii_whitespace();
    let (method, authority, version) = (parts.next()?, parts.next()?, parts.next()?);
    if method != "CONNECT" || !version.starts_with("HTTP/1.") || parts.next().is_some() {
        return None;
    }
    let (host, port) = authority.rsplit_once(':')?;
    Some((host.to_owned(), port.parse().ok()?))
}

async fn connection(
    mut stream: TcpStream,
    _peer: SocketAddr,
    route: &EgressRoute,
    failure: &Mutex<Option<EgressError>>,
    stop: CancellationToken,
) {
    let head = tokio::time::timeout(HEAD_WAIT, read_head(&mut stream)).await;
    let Ok(Some(head)) = head else {
        let _ = refuse(stream, "400 Bad Request", "unreadable request").await;
        return;
    };
    let Some((host, port)) = parse_connect(&head) else {
        let _ = refuse(
            stream,
            "405 Method Not Allowed",
            "this proxy only tunnels CONNECT requests to model providers",
        )
        .await;
        return;
    };
    let target = allow::check_request(&host, &route.declared);
    let target = match target {
        Ok(target) if port == EGRESS_PORT => target,
        _ => {
            let _ = refuse(
                stream,
                "403 Forbidden",
                "this proxy only reaches the model hosts of the provider config, on port 443",
            )
            .await;
            return;
        }
    };
    let opened = tokio::select! {
        _ = stop.cancelled() => return,
        opened = route.open(&target) => opened,
    };
    match opened {
        Ok((reader, writer)) => {
            if let Ok(mut failure) = failure.lock() {
                *failure = None;
            }
            if stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .is_err()
            {
                return;
            }
            pump(stream, reader, writer, stop).await;
        }
        Err(error) => {
            let message = error.to_string();
            let status = match error.kind {
                EgressErrorKind::Refused { .. } | EgressErrorKind::NotPaired => "403 Forbidden",
                _ => "502 Bad Gateway",
            };
            if let Ok(mut failure) = failure.lock() {
                *failure = Some(error);
            }
            let _ = refuse(stream, status, &message).await;
        }
    }
}

/// The local proxy for one route, started when the first request needs it and
/// stopped when the last holder lets go. Hold it as long as the HTTP client
/// that uses it.
pub struct EgressHandle {
    route: EgressRoute,
    proxy: Mutex<Option<Arc<EgressProxy>>>,
    failure: Arc<Mutex<Option<EgressError>>>,
}

impl EgressHandle {
    pub fn new(route: EgressRoute) -> Arc<Self> {
        Arc::new(Self {
            route,
            proxy: Mutex::new(None),
            failure: Arc::new(Mutex::new(None)),
        })
    }

    pub fn route(&self) -> &EgressRoute {
        &self.route
    }

    /// The running proxy, started on first use.
    pub fn proxy(&self) -> std::io::Result<Arc<EgressProxy>> {
        let mut slot = self
            .proxy
            .lock()
            .map_err(|_| std::io::Error::other("egress proxy lock poisoned"))?;
        if let Some(proxy) = slot.as_ref() {
            return Ok(proxy.clone());
        }
        let proxy = Arc::new(EgressProxy::start(
            self.route.clone(),
            self.failure.clone(),
        )?);
        *slot = Some(proxy.clone());
        Ok(proxy)
    }

    /// The proxy URL for a client's proxy setting. If the proxy cannot start
    /// the URL points at a closed port and the failure is recorded: traffic
    /// never falls back to a direct connection.
    pub fn proxy_url(&self) -> String {
        match self.proxy() {
            Ok(proxy) => proxy.url(),
            Err(error) => {
                if let Ok(mut failure) = self.failure.lock() {
                    *failure = Some(EgressError {
                        device_id: self.route.device.clone(),
                        device: self.route.device.clone(),
                        kind: EgressErrorKind::Local(format!(
                            "could not start the local proxy: {error}"
                        )),
                    });
                }
                "http://127.0.0.1:9".to_owned()
            }
        }
    }

    /// Why the latest connection through this route failed, until one succeeds.
    pub fn failure(&self) -> Option<EgressError> {
        self.failure.lock().ok().and_then(|failure| failure.clone())
    }
}

impl std::fmt::Debug for EgressHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EgressHandle")
            .field("device", &self.route.device)
            .finish_non_exhaustive()
    }
}
