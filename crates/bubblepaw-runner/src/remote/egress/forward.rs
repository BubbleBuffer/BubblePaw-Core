//! The forwarding side: a paired device that has been granted egress asks for
//! one connection to a model provider; this host connects TCP and copies bytes.
//! TLS starts and ends on the requesting device, so this host sees only
//! ciphertext, never the prompt, the reply or the API key.
use super::allow::{self, EGRESS_PORT, Refusal};
use super::pump::pump;
use super::wire::{Hello, Reply};
use crate::remote::adapter::{WRITE_TIMEOUT, next_message};
use async_trait::async_trait;
use bubblepaw_core::CancellationToken;
use bubblepaw_remote::frame::{FrameReader, FrameWriter};
use bubblepaw_remote::tunnel::{TunnelReader, TunnelWriter};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpStream;

/// How long a request may take to arrive.
const HELLO_WAIT: Duration = Duration::from_secs(10);
/// Longest request accepted.
const HELLO_LIMIT: usize = 16 * 1024;
/// How long name resolution may take.
const RESOLVE_WAIT: Duration = Duration::from_secs(10);
/// How long a refused requester may take to read the refusal and hang up.
const REFUSAL_LINGER: Duration = Duration::from_secs(3);
/// How long connecting to the provider may take.
const CONNECT_WAIT: Duration = Duration::from_secs(15);

/// How the forwarder reaches a provider host. The default resolves the name
/// and refuses anything that is not a public address; tests supply their own
/// to reach a local fake provider.
#[async_trait]
pub trait EgressDial: Send + Sync {
    /// Connect to `host` (already checked by [`allow::check_request`]) on the
    /// egress port.
    async fn dial(&self, host: &str) -> Result<TcpStream, Refusal>;
}

/// Resolves with the system resolver and connects only to public addresses.
pub struct SystemDial;

#[async_trait]
impl EgressDial for SystemDial {
    async fn dial(&self, host: &str) -> Result<TcpStream, Refusal> {
        let resolved =
            tokio::time::timeout(RESOLVE_WAIT, tokio::net::lookup_host((host, EGRESS_PORT)))
                .await
                .map_err(|_| {
                    Refusal::new("unreachable", format!("looking up \"{host}\" timed out"))
                })?
                .map_err(|error| {
                    Refusal::new(
                        "unreachable",
                        format!("could not look up \"{host}\": {error}"),
                    )
                })?;
        let addresses: Vec<SocketAddr> = resolved.collect();
        let addresses = allow::check_resolved(host, &addresses)?;
        let mut last = None;
        let connect = async {
            for address in addresses {
                match TcpStream::connect(address).await {
                    Ok(stream) => return Ok(stream),
                    Err(error) => last = Some(error),
                }
            }
            Err(last)
        };
        match tokio::time::timeout(CONNECT_WAIT, connect).await {
            Ok(Ok(stream)) => {
                let _ = stream.set_nodelay(true);
                Ok(stream)
            }
            Ok(Err(error)) => Err(Refusal::new(
                "unreachable",
                format!(
                    "could not connect to \"{host}\": {}",
                    error.map(|error| error.to_string()).unwrap_or_default()
                ),
            )),
            Err(_) => Err(Refusal::new(
                "unreachable",
                format!("connecting to \"{host}\" timed out"),
            )),
        }
    }
}

/// Serve one admitted egress tunnel. `gate` is the caller's decision whether
/// this device may use egress now (the host user's grant at pairing, and room
/// for another session); an `Err` is sent as the refusal.
pub(crate) async fn serve<R, W>(
    mut reader: TunnelReader<R>,
    mut writer: TunnelWriter<W>,
    gate: Result<(), Refusal>,
    dial: &dyn EgressDial,
    stop: CancellationToken,
) where
    R: FrameReader + 'static,
    W: FrameWriter + 'static,
{
    let hello = tokio::select! {
        _ = stop.cancelled() => return,
        hello = tokio::time::timeout(HELLO_WAIT, next_message(&mut reader)) => hello,
    };
    let Ok(Some(hello)) = hello else { return };
    let outcome = async {
        gate?;
        if hello.len() > HELLO_LIMIT {
            return Err(Refusal::new("protocol", "the request is oversized"));
        }
        let hello: Hello = serde_json::from_slice(&hello)
            .map_err(|_| Refusal::new("protocol", "the request is not understood"))?;
        if hello.v != 1 {
            return Err(Refusal::new("protocol", "unsupported egress version"));
        }
        let target = allow::check_request(&hello.target, &hello.hosts)?;
        tokio::select! {
            _ = stop.cancelled() => Err(Refusal::new("unreachable", "the host is shutting down")),
            stream = dial.dial(&target) => stream,
        }
    }
    .await;
    let reply = match &outcome {
        Ok(_) => Reply::accepted(),
        Err(refusal) => Reply::refused(refusal),
    };
    let sent = match serde_json::to_vec(&reply) {
        Ok(bytes) => tokio::time::timeout(WRITE_TIMEOUT, writer.send(&bytes)).await,
        Err(_) => return,
    };
    match (outcome, sent) {
        (Ok(stream), Ok(Ok(()))) => pump(stream, reader, writer, stop).await,
        (Err(_), Ok(Ok(()))) => {
            // Keep the tunnel open until the requester has read the refusal
            // and hung up, so closing it cannot overtake the answer.
            let _ = tokio::time::timeout(REFUSAL_LINGER, next_message(&mut reader)).await;
        }
        _ => {}
    }
}
