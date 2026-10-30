//! Five-minute, single-use XXpsk3 pairing with an explicit host approval step.

use std::time::{Duration, Instant};

use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::device::{DeviceId, DeviceKey, noise_public_key};
use crate::frame::FrameIo;
use crate::store::{PairStore, PairedClient, PairedHost, PeerRecord, Profile, validate_name};
use crate::trust::AuthorizedPeer;
use crate::tunnel::{Side, Tunnel, exchange};
use crate::{Error, Result};

mod code;
pub use code::PairingCode;

const PROTOCOL: &[u8] = b"bubblepaw-pair-v1";
const READY: u8 = 0;
const WRONG: u8 = 1;
const EXPIRED: u8 = 2;
const REUSED: u8 = 3;
const PAIR_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    public_key: [u8; 32],
    name: String,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
enum Decision {
    Approved { profile: Profile },
    Declined,
    Expired,
    StorageFailed,
}

/// Address and expected host ID come from the caller's discovery or manual input.
pub struct ClientOptions {
    pub host_id: DeviceId,
    pub reach_address: String,
    pub client_name: String,
}

/// Pairing is not persisted until the host explicitly approves this value.
pub struct PendingHost<T> {
    tunnel: Tunnel<T>,
    comparison: String,
    client: Identity,
    deadline: Instant,
}

/// Display `comparison()` to the client user before awaiting the host decision.
pub struct PendingClient<T> {
    tunnel: Tunnel<T>,
    comparison: String,
    host: PairedHost,
}

/// Starts the host side; at most one operation can borrow a code at a time.
/// Successful proof of the PSK consumes the code, including if approval is declined.
pub async fn accept<T: FrameIo>(
    mut io: T,
    key: &DeviceKey,
    ticket: &mut PairingCode,
    host_name: &str,
) -> Result<PendingHost<T>> {
    validate_name(host_name)?;
    if ticket.used {
        send_status(&mut io, REUSED).await;
        return Err(Error::ReusedCode);
    }
    if ticket.expired() {
        send_status(&mut io, EXPIRED).await;
        return Err(Error::ExpiredCode);
    }
    let deadline = ticket.deadline;
    let mut hello = vec![READY];
    hello.extend_from_slice(PROTOCOL);
    hello.extend_from_slice(key.id().to_string().as_bytes());
    tokio::time::timeout_at(
        tokio::time::Instant::from_std(deadline),
        io.send_frame(&hello),
    )
    .await
    .map_err(|_| Error::ExpiredCode)??;
    let code = Zeroizing::new(ticket.code().to_owned());
    let host_id = key.id();
    let result = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), async {
        let psk = code::derive(code, host_id).await?;
        exchange(&mut io, key, Side::Responder, Some(&psk)).await
    })
    .await;
    let handshake = match result {
        Ok(Ok(handshake)) if !ticket.expired() => handshake,
        Ok(Ok(_)) | Err(_) => {
            send_status(&mut io, EXPIRED).await;
            return Err(Error::ExpiredCode);
        }
        Ok(Err(error)) => {
            send_status(&mut io, WRONG).await;
            return match error {
                Error::Noise(_) => Err(Error::WrongCode),
                other => Err(other),
            };
        }
    };
    ticket.used = true;
    tokio::time::timeout_at(
        tokio::time::Instant::from_std(deadline),
        io.send_frame(&[READY]),
    )
    .await
    .map_err(|_| Error::ExpiredCode)??;
    let comparison = code::comparison(&handshake.handshake_hash);
    let peer_static = handshake.peer_static;
    let mut tunnel = Tunnel::from_handshake(io, handshake);
    let client = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), async {
        send_identity(&mut tunnel, key, host_name).await?;
        receive_identity(&mut tunnel, &peer_static).await
    })
    .await
    .map_err(|_| Error::ExpiredCode)??;
    Ok(PendingHost {
        tunnel,
        comparison,
        client,
        deadline,
    })
}

/// Completes the PSK proof and returns the same comparison number as the host.
/// No identity is saved until `PendingClient::finish` receives host approval.
pub async fn connect<T: FrameIo>(
    io: T,
    key: &DeviceKey,
    code: &str,
    options: ClientOptions,
) -> Result<PendingClient<T>> {
    let code = code::normalize(code)?;
    validate_name(&options.client_name)?;
    tokio::time::timeout(PAIR_TIMEOUT, connect_inner(io, key, code, options))
        .await
        .map_err(|_| Error::ExpiredCode)?
}

async fn connect_inner<T: FrameIo>(
    mut io: T,
    key: &DeviceKey,
    code: Zeroizing<String>,
    options: ClientOptions,
) -> Result<PendingClient<T>> {
    let hello = io.recv_frame().await?;
    check_status(&hello)?;
    let expected = [PROTOCOL, options.host_id.to_string().as_bytes()].concat();
    if hello.get(1..) != Some(expected.as_slice()) {
        return Err(Error::Protocol("pairing protocol or host ID mismatch"));
    }
    let psk = code::derive(code, options.host_id).await?;
    let handshake = exchange(&mut io, key, Side::Initiator, Some(&psk))
        .await
        .map_err(|error| match error {
            Error::Noise(_) => Error::WrongCode,
            other => other,
        })?;
    let status = io.recv_frame().await?;
    check_status(&status)?;
    if status != [READY] {
        return Err(Error::Protocol("invalid pairing handshake status"));
    }
    let comparison = code::comparison(&handshake.handshake_hash);
    let peer_static = handshake.peer_static;
    let mut tunnel = Tunnel::from_handshake(io, handshake);
    let identity = receive_identity(&mut tunnel, &peer_static).await?;
    let host = PairedHost {
        id: options.host_id.to_string(),
        public_key: identity.public_key,
        name: identity.name,
        reach_address: options.reach_address,
    };
    host.validate()?;
    send_identity(&mut tunnel, key, &options.client_name).await?;
    Ok(PendingClient {
        tunnel,
        comparison,
        host,
    })
}

fn check_status(bytes: &[u8]) -> Result<()> {
    match bytes.first() {
        Some(&READY) => Ok(()),
        Some(&WRONG) if bytes.len() == 1 => Err(Error::WrongCode),
        Some(&EXPIRED) if bytes.len() == 1 => Err(Error::ExpiredCode),
        Some(&REUSED) if bytes.len() == 1 => Err(Error::ReusedCode),
        _ => Err(Error::Protocol("invalid pairing status")),
    }
}

// Diagnostics must never let an unresponsive peer keep a pairing attempt alive.
async fn send_status<T: FrameIo>(io: &mut T, status: u8) {
    let _ = tokio::time::timeout(Duration::from_secs(1), io.send_frame(&[status])).await;
}

async fn send_decision<T: FrameIo>(tunnel: &mut Tunnel<T>, decision: &Decision) -> Result<()> {
    let bytes = serde_json::to_vec(decision)?;
    tokio::time::timeout(Duration::from_secs(1), tunnel.send(&bytes))
        .await
        .map_err(|_| Error::IdleTimeout)?
}

async fn send_identity<T: FrameIo>(
    tunnel: &mut Tunnel<T>,
    key: &DeviceKey,
    name: &str,
) -> Result<()> {
    tunnel
        .send(&serde_json::to_vec(&Identity {
            public_key: key.public_key(),
            name: name.to_owned(),
        })?)
        .await
}

async fn receive_identity<T: FrameIo>(
    tunnel: &mut Tunnel<T>,
    peer_static: &[u8; 32],
) -> Result<Identity> {
    let bytes = tunnel.recv().await?;
    if bytes.len() > 2048 {
        return Err(Error::Protocol("pairing identity is oversized"));
    }
    let identity: Identity = serde_json::from_slice(&bytes)?;
    validate_name(&identity.name)?;
    if noise_public_key(&identity.public_key)? != *peer_static {
        return Err(Error::UnpairedPeer);
    }
    Ok(identity)
}

impl<T: FrameIo> PendingHost<T> {
    pub fn comparison(&self) -> &str {
        &self.comparison
    }

    pub fn client_public_key(&self) -> &[u8; 32] {
        &self.client.public_key
    }

    pub fn client_suggested_name(&self) -> &str {
        &self.client.name
    }

    /// Call only after the host user confirms the number on both devices.
    /// The host chooses the saved name and permission profile.
    pub async fn approve(
        mut self,
        name: String,
        profile: Profile,
        store: &mut PairStore<PairedClient>,
    ) -> Result<Tunnel<T>> {
        if Instant::now() >= self.deadline {
            let _ = send_decision(&mut self.tunnel, &Decision::Expired).await;
            return Err(Error::ExpiredCode);
        }
        let peer = PairedClient {
            public_key: self.client.public_key,
            name,
            profile,
        };
        if let Err(error) = store.insert(peer) {
            let _ = send_decision(&mut self.tunnel, &Decision::StorageFailed).await;
            return Err(error);
        }
        self.tunnel.set_authorization(AuthorizedPeer {
            id: DeviceId::from_public_key(&self.client.public_key),
            public_key: self.client.public_key,
            profile,
        });
        send_decision(&mut self.tunnel, &Decision::Approved { profile }).await?;
        Ok(self.tunnel)
    }

    pub async fn decline(mut self) -> Result<()> {
        send_decision(&mut self.tunnel, &Decision::Declined).await?;
        Err(Error::ComparisonDeclined)
    }
}

impl<T: FrameIo> PendingClient<T> {
    pub fn comparison(&self) -> &str {
        &self.comparison
    }

    pub fn host(&self) -> &PairedHost {
        &self.host
    }

    pub async fn finish(mut self, store: &mut PairStore<PairedHost>) -> Result<Tunnel<T>> {
        let bytes = tokio::time::timeout(PAIR_TIMEOUT, self.tunnel.recv())
            .await
            .map_err(|_| Error::ExpiredCode)??;
        if bytes.len() > 256 {
            return Err(Error::Protocol("pairing decision is oversized"));
        }
        match serde_json::from_slice::<Decision>(&bytes)? {
            Decision::Approved { .. } => {
                let authorized = AuthorizedPeer {
                    id: self.host.id(),
                    public_key: self.host.public_key,
                    profile: Profile::Full,
                };
                store.insert(self.host)?;
                self.tunnel.set_authorization(authorized);
            }
            Decision::Declined => return Err(Error::ComparisonDeclined),
            Decision::Expired => return Err(Error::ExpiredCode),
            Decision::StorageFailed => {
                return Err(Error::Protocol("host could not save the pairing"));
            }
        }
        Ok(self.tunnel)
    }
}
