//! Concurrent receive and send halves of one admitted tunnel.
//!
//! Noise keeps one cipher state per direction, so a reader and a writer can run
//! at the same time. Each half is the only user of its direction: records leave
//! in the order their nonces were taken and are read in the order they arrive.
//! The shared state is locked only while a record is encrypted or decrypted.
//! As on a whole tunnel, a failed or cancelled operation makes that half
//! unusable; reconnect instead of guessing whether a record was consumed.
use std::sync::{Arc, Mutex};

use super::{MAX_MESSAGE_LEN, Tunnel, append_chunk, checked, chunks};
use crate::frame::{FrameReader, FrameWriter, MAX_FRAME_LEN, SplitFrames};
use crate::{Error, Result};

type Transport = Arc<Mutex<snow::TransportState>>;

/// The two halves of a tunnel on carrier `T`.
pub type SplitTunnel<T> = (
    TunnelReader<<T as SplitFrames>::Reader>,
    TunnelWriter<<T as SplitFrames>::Writer>,
);

/// The receive half from [`Tunnel::split`].
pub struct TunnelReader<R> {
    io: R,
    transport: Transport,
    usable: bool,
}

/// The send half from [`Tunnel::split`].
pub struct TunnelWriter<W> {
    io: W,
    transport: Transport,
    usable: bool,
}

impl<T: SplitFrames> Tunnel<T> {
    /// Split the tunnel so one task can receive while another sends. Read
    /// `authorization()` first: the halves carry only the encrypted stream.
    pub fn split(self) -> Result<SplitTunnel<T>> {
        if !self.usable {
            return Err(unusable());
        }
        let (reader, writer) = self.io.split_frames();
        let transport = Arc::new(Mutex::new(self.transport));
        Ok((
            TunnelReader {
                io: reader,
                transport: transport.clone(),
                usable: true,
            },
            TunnelWriter {
                io: writer,
                transport,
                usable: true,
            },
        ))
    }
}

fn unusable() -> Error {
    Error::Protocol("tunnel is unusable after an interrupted operation")
}

fn locked(transport: &Transport) -> Result<std::sync::MutexGuard<'_, snow::TransportState>> {
    transport
        .lock()
        .map_err(|_| Error::Protocol("tunnel cipher state is poisoned"))
}

impl<R: FrameReader> TunnelReader<R> {
    /// The next whole message (empty messages included).
    pub async fn recv(&mut self) -> Result<Vec<u8>> {
        if !self.usable {
            return Err(unusable());
        }
        self.usable = false;
        let mut message = Vec::new();
        loop {
            let frame = checked(self.io.recv_frame().await?)?;
            let mut buffer = vec![0; MAX_FRAME_LEN];
            let length = locked(&self.transport)?.read_message(&frame, &mut buffer)?;
            if append_chunk(&mut message, &buffer[..length])? {
                self.usable = true;
                return Ok(message);
            }
        }
    }
}

impl<W: FrameWriter> TunnelWriter<W> {
    /// Send one whole message of at most 16 MiB (empty messages included).
    pub async fn send(&mut self, payload: &[u8]) -> Result<()> {
        if !self.usable {
            return Err(unusable());
        }
        if payload.len() > MAX_MESSAGE_LEN {
            return Err(Error::OversizedFrame);
        }
        self.usable = false;
        for plaintext in chunks(payload) {
            let mut buffer = vec![0; MAX_FRAME_LEN];
            let length = locked(&self.transport)?.write_message(&plaintext, &mut buffer)?;
            self.io.send_frame(&buffer[..length]).await?;
        }
        self.usable = true;
        Ok(())
    }
}
