//! Carriers a tunnel runs on: a relay session, or a direct WebSocket with an
//! idle limit (both ends send a keepalive every 20 seconds, so a direct socket
//! that stays silent much longer than that is dead, even when TCP has not
//! noticed yet).
use async_trait::async_trait;
use bubblepaw_remote::direct::DirectSocket;
use bubblepaw_remote::frame::{FrameIo, FrameReader, FrameWriter, SplitFrames};
use bubblepaw_remote::relay::{RelayReader, RelaySession, RelayWriter};
use bubblepaw_remote::{Error, Result};
use std::time::Duration;

/// How long a direct socket may stay silent; six keepalive intervals.
pub(super) const DIRECT_IDLE: Duration = Duration::from_secs(120);

/// A carrier whose receives fail after `idle` without a frame.
pub(super) struct Idle<T> {
    inner: T,
    idle: Duration,
}

impl<T> Idle<T> {
    pub(super) fn new(inner: T, idle: Duration) -> Self {
        Self { inner, idle }
    }
}

async fn timed(idle: Duration, frame: impl Future<Output = Result<Vec<u8>>>) -> Result<Vec<u8>> {
    tokio::time::timeout(idle, frame)
        .await
        .map_err(|_| Error::IdleTimeout)?
}

#[async_trait]
impl<T: FrameIo> FrameIo for Idle<T> {
    async fn send_frame(&mut self, frame: &[u8]) -> Result<()> {
        self.inner.send_frame(frame).await
    }

    async fn recv_frame(&mut self) -> Result<Vec<u8>> {
        timed(self.idle, self.inner.recv_frame()).await
    }
}

/// The receive half of a split [`Idle`] carrier.
pub(super) struct IdleReader<R> {
    inner: R,
    idle: Duration,
}

#[async_trait]
impl<R: FrameReader> FrameReader for IdleReader<R> {
    async fn recv_frame(&mut self) -> Result<Vec<u8>> {
        timed(self.idle, self.inner.recv_frame()).await
    }
}

impl<T: SplitFrames> SplitFrames for Idle<T> {
    type Reader = IdleReader<T::Reader>;
    type Writer = T::Writer;

    fn split_frames(self) -> (Self::Reader, Self::Writer) {
        let (reader, writer) = self.inner.split_frames();
        (
            IdleReader {
                inner: reader,
                idle: self.idle,
            },
            writer,
        )
    }
}

/// A device's carrier to a host, through a relay or direct.
pub(super) enum Carrier {
    Relay(RelaySession),
    Direct(Box<Idle<DirectSocket>>),
}

#[async_trait]
impl FrameIo for Carrier {
    async fn send_frame(&mut self, frame: &[u8]) -> Result<()> {
        match self {
            Self::Relay(session) => session.send_frame(frame).await,
            Self::Direct(socket) => socket.send_frame(frame).await,
        }
    }

    async fn recv_frame(&mut self) -> Result<Vec<u8>> {
        match self {
            Self::Relay(session) => session.recv_frame().await,
            Self::Direct(socket) => socket.recv_frame().await,
        }
    }
}

pub(super) enum CarrierReader {
    Relay(RelayReader),
    Direct(<Idle<DirectSocket> as SplitFrames>::Reader),
}

pub(super) enum CarrierWriter {
    Relay(RelayWriter),
    Direct(<Idle<DirectSocket> as SplitFrames>::Writer),
}

#[async_trait]
impl FrameReader for CarrierReader {
    async fn recv_frame(&mut self) -> Result<Vec<u8>> {
        match self {
            Self::Relay(reader) => reader.recv_frame().await,
            Self::Direct(reader) => reader.recv_frame().await,
        }
    }
}

#[async_trait]
impl FrameWriter for CarrierWriter {
    async fn send_frame(&mut self, frame: &[u8]) -> Result<()> {
        match self {
            Self::Relay(writer) => writer.send_frame(frame).await,
            Self::Direct(writer) => writer.send_frame(frame).await,
        }
    }
}

impl SplitFrames for Carrier {
    type Reader = CarrierReader;
    type Writer = CarrierWriter;

    fn split_frames(self) -> (CarrierReader, CarrierWriter) {
        match self {
            Self::Relay(session) => {
                let (reader, writer) = session.split_frames();
                (CarrierReader::Relay(reader), CarrierWriter::Relay(writer))
            }
            Self::Direct(socket) => {
                let (reader, writer) = (*socket).split_frames();
                (CarrierReader::Direct(reader), CarrierWriter::Direct(writer))
            }
        }
    }
}
