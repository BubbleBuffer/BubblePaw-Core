//! How devices reach a host: a relay, or the host's own WebSocket endpoint on
//! 127.0.0.1, optionally published by a managed `cloudflared`.
use super::carrier::{DIRECT_IDLE, Idle};
use super::server::{Inner, handle};
use anyhow::{Context, Result};
use bubblepaw_remote::direct;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use zeroize::Zeroizing;

/// How long a new direct connection may take to finish its WebSocket upgrade.
const UPGRADE_WAIT: Duration = Duration::from_secs(10);
/// Direct connections open at once (handshakes, pairing attempts and tunnels).
const DIRECT_CONNECTIONS: usize = 128;

/// Where a host is reachable.
#[derive(Clone, Debug)]
pub enum RemoteReach {
    /// Register at the relay with this `wss://` (or `ws://`) URL.
    Relay(String),
    /// Serve the host's own WebSocket endpoint on `127.0.0.1:port` (0 picks a
    /// free port) and advertise `address`, by default `ws://127.0.0.1:<port>`.
    /// For a caller that publishes the endpoint itself, and for tests.
    Local { port: u16, address: Option<String> },
    /// Serve the endpoint on 127.0.0.1 and publish it through a Cloudflare
    /// Tunnel run by a managed `cloudflared`.
    Cloudflare(CloudflareTunnel),
}

/// A Cloudflare Tunnel in front of the host's endpoint.
#[derive(Clone, Debug)]
pub struct CloudflareTunnel {
    pub mode: TunnelMode,
    /// Local port of the host's endpoint; 0 picks a free one.
    pub port: u16,
    /// The `cloudflared` program.
    pub cloudflared: PathBuf,
    /// Where the named tunnel's credentials and state live
    /// (`RemotePaths::cloudflare`).
    pub dir: PathBuf,
    /// Set up the named tunnel and its DNS route through the Cloudflare API.
    /// Without it, setup uses `cloudflared` and its login certificate.
    pub api: Option<CloudflareApi>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TunnelMode {
    /// A trycloudflare.com quick tunnel: no account, a new address on every
    /// start, so devices pair again after a restart.
    Quick,
    /// A named tunnel on the user's own domain, reached at `wss://<hostname>`.
    Domain { hostname: String },
}

/// Cloudflare API access for domain setup: a token with Cloudflare Tunnel edit
/// and DNS edit permissions. The token is a secret: never log it.
#[derive(Clone)]
pub struct CloudflareApi {
    pub token: Zeroizing<String>,
    /// `https://api.cloudflare.com/client/v4` unless testing.
    pub base_url: String,
}

/// The Cloudflare API's base URL.
pub const CLOUDFLARE_API: &str = "https://api.cloudflare.com/client/v4";

impl CloudflareApi {
    pub fn new(token: String) -> Self {
        Self {
            token: Zeroizing::new(token),
            base_url: CLOUDFLARE_API.to_owned(),
        }
    }
}

impl std::fmt::Debug for CloudflareApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudflareApi")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

/// Bind the host's endpoint on loopback; never on another interface.
pub(super) fn bind(port: u16) -> Result<TcpListener> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("could not listen on 127.0.0.1:{port}"))?;
    listener.set_nonblocking(true)?;
    TcpListener::from_std(listener).context("could not serve the host endpoint")
}

/// Accept direct connections until the server stops; each one upgrades to a
/// WebSocket and is handled like a relay session.
pub(super) async fn direct_loop(inner: Arc<Inner>, listener: TcpListener) {
    let permits = Arc::new(Semaphore::new(DIRECT_CONNECTIONS));
    loop {
        let accepted = tokio::select! {
            _ = inner.stop.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        let stream = match accepted {
            Ok((stream, _)) => stream,
            Err(error) => {
                tracing::warn!("host endpoint accept failed: {error}");
                tokio::select! {
                    _ = inner.stop.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_millis(100)) => continue,
                }
            }
        };
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            tracing::debug!("host endpoint is at its connection limit");
            continue;
        };
        let inner = inner.clone();
        tokio::spawn(async move {
            let _permit = permit;
            match direct::accept(stream, UPGRADE_WAIT).await {
                Ok(socket) => handle(inner, Idle::new(socket, DIRECT_IDLE)).await,
                Err(error) => tracing::debug!("direct connection ended: {error}"),
            }
        });
    }
}
