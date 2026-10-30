//! Bounded Noise records over byte streams and WebSocket binary messages.
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_tungstenite::{WebSocketStream, tungstenite::protocol::WebSocketConfig};

use crate::{Error, Result};

/// Noise's maximum complete ciphertext record size, including its AEAD tag.
pub const MAX_FRAME_LEN: usize = 65_535;

#[async_trait]
pub trait FrameIo: Send {
    async fn send_frame(&mut self, frame: &[u8]) -> Result<()>;
    async fn recv_frame(&mut self) -> Result<Vec<u8>>;
}

/// A four-byte, big-endian length followed by one bounded record.
pub struct ByteFrames<T> {
    inner: T,
}

impl<T> ByteFrames<T> {
    pub fn new(inner: T) -> Self {
        Self { inner }
    }

    pub fn into_inner(self) -> T {
        self.inner
    }
}

#[async_trait]
impl<T: AsyncRead + AsyncWrite + Unpin + Send> FrameIo for ByteFrames<T> {
    async fn send_frame(&mut self, frame: &[u8]) -> Result<()> {
        if frame.len() > MAX_FRAME_LEN {
            return Err(Error::OversizedFrame);
        }
        self.inner.write_u32(frame.len() as u32).await?;
        self.inner.write_all(frame).await?;
        self.inner.flush().await?;
        Ok(())
    }

    async fn recv_frame(&mut self) -> Result<Vec<u8>> {
        let length = self.inner.read_u32().await? as usize;
        if length > MAX_FRAME_LEN {
            return Err(Error::OversizedFrame);
        }
        let mut frame = vec![0; length];
        self.inner.read_exact(&mut frame).await?;
        Ok(frame)
    }
}

/// Use this when creating the WebSocket, before it can buffer incoming frames.
/// The wrapper also checks binary message lengths, but cannot retroactively
/// change tungstenite's allocation limits on an existing WebSocketStream.
pub fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME_LEN))
        .max_frame_size(Some(MAX_FRAME_LEN))
}

/// One Noise record per WebSocket binary message. Construct the underlying
/// socket with [`websocket_config`] to bound fragmented message reassembly.
pub struct WebSocketFrames<T> {
    inner: WebSocketStream<T>,
}

impl<T> WebSocketFrames<T> {
    pub fn new(inner: WebSocketStream<T>) -> Self {
        Self { inner }
    }

    pub fn into_inner(self) -> WebSocketStream<T> {
        self.inner
    }
}

#[async_trait]
impl<T: AsyncRead + AsyncWrite + Unpin + Send> FrameIo for WebSocketFrames<T> {
    async fn send_frame(&mut self, frame: &[u8]) -> Result<()> {
        if frame.len() > MAX_FRAME_LEN {
            return Err(Error::OversizedFrame);
        }
        self.inner.send(frame.to_vec().into()).await?;
        Ok(())
    }

    async fn recv_frame(&mut self) -> Result<Vec<u8>> {
        use tokio_tungstenite::tungstenite::Message;
        let config = self.inner.get_config();
        if config
            .max_message_size
            .is_none_or(|limit| limit > MAX_FRAME_LEN)
            || config
                .max_frame_size
                .is_none_or(|limit| limit > MAX_FRAME_LEN)
        {
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
                Message::Ping(_) => self.inner.flush().await?,
                Message::Pong(_) => {}
                Message::Close(_) => return Err(Error::Closed),
                _ => return Err(Error::Protocol("expected a WebSocket binary message")),
            }
        }
        Err(Error::Closed)
    }
}
