//! Noise XX transport with paired-key admission and bounded message chunking.
//!
//! Application records contain an encrypted continuation byte followed by at
//! most 65,518 payload bytes. Whole messages are limited to 16 MiB, independently
//! of the 65,535-byte ciphertext record limit. Non-final chunks must be full.
//! A failed or cancelled operation invalidates the tunnel: callers must make a
//! fresh connection instead of guessing whether a record or nonce was consumed.
use crate::{
    Error, Result,
    device::{DeviceId, DeviceKey, noise_public_key},
    frame::{FrameIo, MAX_FRAME_LEN},
    trust::{AuthorizedPeer, PeerAuthorizer, PublicKey},
};

pub const MAX_MESSAGE_LEN: usize = 16 * 1024 * 1024;
pub const MAX_CHUNK_PAYLOAD: usize = MAX_FRAME_LEN - 16 - 1;
const PROLOGUE: &[u8] = b"bubblepaw-remote/noise/v1";
const TRUSTED: &[u8] = b"bubblepaw-remote/trusted/v1";
const REJECTED: &[u8] = b"bubblepaw-remote/rejected/v1";

#[derive(Clone, Copy, Debug)]
pub enum Side {
    Initiator,
    Responder,
}

pub struct HandshakeResult {
    pub transport: snow::TransportState,
    pub peer_static: [u8; 32],
    pub handshake_hash: [u8; 32],
}

/// Raw XX (or XXpsk3) for the pairing flow. This proves possession of the static
/// key, but does not decide whether the peer is authorized. No application data
/// is accepted during the handshake. The caller owns pairing confirmation.
pub async fn exchange<T: FrameIo>(
    io: &mut T,
    key: &DeviceKey,
    side: Side,
    psk: Option<&[u8; 32]>,
) -> Result<HandshakeResult> {
    let pattern = if psk.is_some() {
        "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s"
    } else {
        "Noise_XX_25519_ChaChaPoly_BLAKE2s"
    };
    let private = key.noise_private_key();
    let mut builder = snow::Builder::new(pattern.parse()?)
        .local_private_key(&private[..])
        .prologue(PROLOGUE);
    if let Some(psk) = psk {
        builder = builder.psk(3, psk);
    }
    let mut handshake = match side {
        Side::Initiator => builder.build_initiator()?,
        Side::Responder => builder.build_responder()?,
    };
    let mut buffer = vec![0; MAX_FRAME_LEN];
    while !handshake.is_handshake_finished() {
        if handshake.is_my_turn() {
            let length = handshake.write_message(&[], &mut buffer)?;
            io.send_frame(&buffer[..length]).await?;
        } else {
            let frame = checked_frame(io).await?;
            if handshake.read_message(&frame, &mut buffer)? != 0 {
                return Err(Error::Protocol("unexpected Noise handshake payload"));
            }
        }
    }
    let peer_static = handshake
        .get_remote_static()
        .ok_or(Error::Protocol("Noise handshake omitted peer static key"))?
        .try_into()
        .map_err(|_| Error::Protocol("invalid Noise static key length"))?;
    let handshake_hash = handshake
        .get_handshake_hash()
        .try_into()
        .map_err(|_| Error::Protocol("invalid Noise handshake hash length"))?;
    Ok(HandshakeResult {
        peer_static,
        handshake_hash,
        transport: handshake.into_transport_mode()?,
    })
}

async fn checked_frame<T: FrameIo>(io: &mut T) -> Result<Vec<u8>> {
    let frame = io.recv_frame().await?;
    if frame.len() > MAX_FRAME_LEN {
        return Err(Error::OversizedFrame);
    }
    Ok(frame)
}

pub struct Tunnel<T> {
    io: T,
    transport: snow::TransportState,
    peer_static: [u8; 32],
    handshake_hash: [u8; 32],
    authorization: Option<AuthorizedPeer>,
    usable: bool,
}

impl<T: FrameIo> Tunnel<T> {
    /// The caller must have completed pairing authorization before using this.
    /// Ordinary reconnects should use [`connect`] or [`accept`] instead.
    pub fn from_handshake(io: T, result: HandshakeResult) -> Self {
        Self {
            io,
            transport: result.transport,
            peer_static: result.peer_static,
            handshake_hash: result.handshake_hash,
            authorization: None,
            usable: true,
        }
    }

    pub fn peer_static(&self) -> &[u8; 32] {
        &self.peer_static
    }

    pub fn handshake_hash(&self) -> &[u8; 32] {
        &self.handshake_hash
    }

    /// The local policy grant at admission, including the bound Ed25519 identity.
    /// Raw pairing tunnels created with `from_handshake` have no policy grant.
    pub fn authorization(&self) -> Option<&AuthorizedPeer> {
        self.authorization.as_ref()
    }

    pub(crate) fn set_authorization(&mut self, peer: AuthorizedPeer) {
        self.authorization = Some(peer);
    }

    pub fn into_inner(self) -> T {
        self.io
    }

    async fn write_record(&mut self, plaintext: &[u8]) -> Result<()> {
        let mut buffer = vec![0; MAX_FRAME_LEN];
        let length = self.transport.write_message(plaintext, &mut buffer)?;
        self.io.send_frame(&buffer[..length]).await
    }

    async fn read_record(&mut self) -> Result<Vec<u8>> {
        let frame = checked_frame(&mut self.io).await?;
        let mut buffer = vec![0; MAX_FRAME_LEN];
        let length = self.transport.read_message(&frame, &mut buffer)?;
        buffer.truncate(length);
        Ok(buffer)
    }

    pub async fn send(&mut self, payload: &[u8]) -> Result<()> {
        if !self.usable {
            return Err(Error::Protocol(
                "tunnel is unusable after an interrupted operation",
            ));
        }
        if payload.len() > MAX_MESSAGE_LEN {
            return Err(Error::OversizedFrame);
        }
        self.usable = false;
        let result = self.send_payload(payload).await;
        self.usable = result.is_ok();
        result
    }

    async fn send_payload(&mut self, payload: &[u8]) -> Result<()> {
        let mut remaining = payload;
        loop {
            let length = remaining.len().min(MAX_CHUNK_PAYLOAD);
            let more = remaining.len() > length;
            let mut plaintext = Vec::with_capacity(length + 1);
            plaintext.push(u8::from(more));
            plaintext.extend_from_slice(&remaining[..length]);
            self.write_record(&plaintext).await?;
            remaining = &remaining[length..];
            if !more {
                return Ok(());
            }
        }
    }

    pub async fn recv(&mut self) -> Result<Vec<u8>> {
        if !self.usable {
            return Err(Error::Protocol(
                "tunnel is unusable after an interrupted operation",
            ));
        }
        self.usable = false;
        let result = self.recv_payload().await;
        self.usable = result.is_ok();
        result
    }

    async fn recv_payload(&mut self) -> Result<Vec<u8>> {
        let mut message = Vec::new();
        loop {
            let plaintext = self.read_record().await?;
            let (&continuation, payload) = plaintext
                .split_first()
                .ok_or(Error::Protocol("missing encrypted chunk marker"))?;
            if continuation > 1 || (continuation == 1 && payload.len() != MAX_CHUNK_PAYLOAD) {
                return Err(Error::Protocol("invalid encrypted chunk marker or length"));
            }
            if message.len() + payload.len() > MAX_MESSAGE_LEN {
                return Err(Error::OversizedFrame);
            }
            let required = message.len() + payload.len();
            if required > message.capacity() {
                let capacity = required
                    .max(message.capacity().saturating_mul(2))
                    .min(MAX_MESSAGE_LEN);
                message.reserve_exact(capacity - message.len());
            }
            message.extend_from_slice(payload);
            if continuation == 0 {
                return Ok(message);
            }
        }
    }
}

/// Admit the peer through caller-owned policy after its Ed25519 identity has
/// been bound to the Noise static key. Key slices remain a `Full` allowlist.
pub async fn connect<T: FrameIo, A: PeerAuthorizer + ?Sized>(
    io: T,
    key: &DeviceKey,
    authorizer: &A,
) -> Result<Tunnel<T>> {
    admitted(io, key, authorizer, Side::Initiator).await
}

pub async fn accept<T: FrameIo, A: PeerAuthorizer + ?Sized>(
    io: T,
    key: &DeviceKey,
    authorizer: &A,
) -> Result<Tunnel<T>> {
    admitted(io, key, authorizer, Side::Responder).await
}

async fn admitted<T: FrameIo, A: PeerAuthorizer + ?Sized>(
    mut io: T,
    key: &DeviceKey,
    authorizer: &A,
    side: Side,
) -> Result<Tunnel<T>> {
    let handshake = exchange(&mut io, key, side, None).await?;
    let mut tunnel = Tunnel::from_handshake(io, handshake);
    // Identity records precede policy decisions, with responder-first writes so
    // admission also works on carriers with a single-byte write buffer.
    if matches!(side, Side::Responder) {
        tunnel.write_record(&key.public_key()).await?;
    }
    let public_key: PublicKey = tunnel
        .read_record()
        .await?
        .try_into()
        .map_err(|_| Error::Protocol("invalid peer Ed25519 identity length"))?;
    if noise_public_key(&public_key)? != tunnel.peer_static {
        return Err(Error::UnpairedPeer);
    }
    if matches!(side, Side::Initiator) {
        tunnel.write_record(&key.public_key()).await?;
    }
    let id = DeviceId::from_public_key(&public_key);
    let profile = authorizer.authorize(&id, &public_key);
    let is_trusted = profile.is_some();
    // The responder confirms first, then the initiator. Ordered confirmation
    // also works on byte streams with tiny write buffers. Neither side can
    // return an application-ready tunnel before learning the other's decision.
    if matches!(side, Side::Responder) {
        if !is_trusted {
            let _ = tunnel.write_record(REJECTED).await;
            return Err(Error::UnpairedPeer);
        }
        tunnel.write_record(TRUSTED).await?;
    }
    let confirmation = tunnel.read_record().await?;
    if confirmation == REJECTED {
        return Err(Error::UnpairedPeer);
    }
    if confirmation != TRUSTED {
        return Err(Error::Protocol(
            "missing encrypted peer admission confirmation",
        ));
    }
    if matches!(side, Side::Initiator) {
        if !is_trusted {
            let _ = tunnel.write_record(REJECTED).await;
            return Err(Error::UnpairedPeer);
        }
        tunnel.write_record(TRUSTED).await?;
    }
    if let Some(profile) = profile {
        tunnel.set_authorization(AuthorizedPeer {
            id,
            public_key,
            profile,
        });
    }
    Ok(tunnel)
}
