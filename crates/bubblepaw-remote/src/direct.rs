//! Direct WebSocket carriers: a host serving its own endpoint (for example
//! behind a Cloudflare Tunnel) and a device dialing it, without a relay.
//!
//! The socket only carries Noise records, one per binary message, exactly as
//! [`WebSocketFrames`] does; whoever terminates TLS or forwards the socket sees
//! ciphertext. Authentication is the tunnel's or pairing's handshake, not the
//! socket.
use std::time::Duration;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::frame::{
    FrameReader, FrameWriter, MAX_FRAME_LEN, SplitFrames, WebSocketFrames, websocket_config,
};
use crate::{Error, Result};

/// A device's socket to a host's direct endpoint.
pub type DirectSocket = WebSocketFrames<MaybeTlsStream<TcpStream>>;

/// Open a WebSocket to a host's direct endpoint (`wss://`, or `ws://` for a
/// loopback address), bounded like every carrier socket.
pub async fn connect(url: &str, timeout: Duration) -> Result<DirectSocket> {
    if !(url.starts_with("wss://") || url.starts_with("ws://")) {
        return Err(Error::Protocol("a direct address is a ws:// or wss:// URL"));
    }
    let (socket, _) = tokio::time::timeout(
        timeout,
        tokio_tungstenite::connect_async_with_config(url, Some(websocket_config()), false),
    )
    .await
    .map_err(|_| Error::IdleTimeout)??;
    Ok(WebSocketFrames::new(socket))
}

/// Complete the WebSocket upgrade of one accepted connection on a host's
/// direct endpoint, bounded like every carrier socket. Any path is accepted.
pub async fn accept<S>(stream: S, timeout: Duration) -> Result<WebSocketFrames<S>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let socket = tokio::time::timeout(
        timeout,
        tokio_tungstenite::accept_async_with_config(stream, Some(websocket_config())),
    )
    .await
    .map_err(|_| Error::IdleTimeout)??;
    Ok(WebSocketFrames::new(socket))
}

/// The receive direction of a split [`WebSocketFrames`]. Ping replies are
/// queued by the socket and leave with the next write or read.
pub struct WebSocketReader<T> {
    inner: SplitStream<WebSocketStream<T>>,
    bounded: bool,
}

/// The send direction of a split [`WebSocketFrames`].
pub struct WebSocketWriter<T> {
    inner: SplitSink<WebSocketStream<T>, Message>,
}

#[async_trait::async_trait]
impl<T: AsyncRead + AsyncWrite + Unpin + Send> FrameReader for WebSocketReader<T> {
    async fn recv_frame(&mut self) -> Result<Vec<u8>> {
        if !self.bounded {
            return Err(Error::Protocol(
                "WebSocket requires bounded frame and message limits",
            ));
        }
        while let Some(message) = self.inner.next().await {
            match message? {
                Message::Binary(bytes) => {
                    if bytes.len() > MAX_FRAME_LEN {
                        return Err(Error::OversizedFrame);
                    }
                    return Ok(bytes.to_vec());
                }
                Message::Ping(_) | Message::Pong(_) => {}
                Message::Close(_) => return Err(Error::Closed),
                _ => return Err(Error::Protocol("expected a WebSocket binary message")),
            }
        }
        Err(Error::Closed)
    }
}

#[async_trait::async_trait]
impl<T: AsyncRead + AsyncWrite + Unpin + Send> FrameWriter for WebSocketWriter<T> {
    async fn send_frame(&mut self, frame: &[u8]) -> Result<()> {
        if frame.len() > MAX_FRAME_LEN {
            return Err(Error::OversizedFrame);
        }
        self.inner.send(frame.to_vec().into()).await?;
        Ok(())
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> SplitFrames for WebSocketFrames<T> {
    type Reader = WebSocketReader<T>;
    type Writer = WebSocketWriter<T>;

    fn split_frames(self) -> (Self::Reader, Self::Writer) {
        let socket = self.into_inner();
        let config = socket.get_config();
        let bounded = config
            .max_message_size
            .is_some_and(|limit| limit <= MAX_FRAME_LEN)
            && config
                .max_frame_size
                .is_some_and(|limit| limit <= MAX_FRAME_LEN);
        let (sink, stream) = socket.split();
        (
            WebSocketReader {
                inner: stream,
                bounded,
            },
            WebSocketWriter { inner: sink },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::DeviceKey;
    use crate::tunnel;

    #[tokio::test]
    async fn a_tunnel_runs_over_a_direct_socket_in_both_directions() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/", listener.local_addr().unwrap());
        let host = DeviceKey::generate().unwrap();
        let device = DeviceKey::generate().unwrap();
        let host_public = [host.public_key()];
        let device_public = [device.public_key()];
        let served = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let socket = accept(stream, Duration::from_secs(5)).await.unwrap();
            let tunnel = tunnel::accept(socket, &host, &device_public[..])
                .await
                .unwrap();
            let (mut reader, mut writer) = tunnel.split().unwrap();
            let message = reader.recv().await.unwrap();
            writer.send(&message).await.unwrap();
            // Wait for the device to close.
            let _ = reader.recv().await;
        });
        let socket = connect(&url, Duration::from_secs(5)).await.unwrap();
        let tunnel = tunnel::connect(socket, &device, &host_public[..])
            .await
            .unwrap();
        let (mut reader, mut writer) = tunnel.split().unwrap();
        let big = vec![7u8; 200_000];
        writer.send(&big).await.unwrap();
        assert_eq!(reader.recv().await.unwrap(), big);
        drop((reader, writer));
        served.await.unwrap();
    }

    #[tokio::test]
    async fn only_websocket_urls_are_dialed() {
        assert!(matches!(
            connect("https://example.invalid", Duration::from_secs(1)).await,
            Err(Error::Protocol(_))
        ));
    }
}
