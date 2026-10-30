use super::{
    Limits,
    wire::{self, Wire},
};
use crate::{Error, Result, device::DeviceId};
use ed25519_dalek::{Signature, VerifyingKey};
use rand_core::{OsRng, RngCore};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::WebSocketStream;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RelayRole {
    Host,
    Client,
}

/// Public device-possession evidence, verified before any admission callback.
/// Account/session context belongs in the per-connection admission object, not
/// in untrusted relay frames. This type deliberately does not implement Debug.
pub struct RelayProof {
    pub public_key: [u8; 32],
    pub challenge: [u8; 32],
    pub signature: [u8; 64],
    pub role: RelayRole,
    pub target: Option<DeviceId>,
}

/// Host-owned policy layered after mandatory Ed25519 possession verification.
/// A route handler may capture its authenticated account/session in this object.
/// Callbacks are synchronous and should finish promptly without logging secrets.
pub trait RelayAdmission: Send + Sync {
    fn admit_host(&self, _host: &DeviceId, _proof: &RelayProof) -> bool {
        true
    }
    fn admit_client(&self, _client: &DeviceId, _proof: &RelayProof) -> bool {
        true
    }
    fn may_connect(&self, _client: &DeviceId, _host: &DeviceId, _proof: &RelayProof) -> bool {
        true
    }
}

/// Standalone policy: any device with a valid possession proof may register or
/// request a connection. The host's encrypted tunnel still authorizes its peer.
#[derive(Default)]
pub struct PermissiveAdmission;
impl RelayAdmission for PermissiveAdmission {}

pub(super) async fn authenticate<S>(
    ws: &mut WebSocketStream<S>,
    limits: &Limits,
) -> Result<RelayProof>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut challenge = [0; 32];
    OsRng
        .try_fill_bytes(&mut challenge)
        .map_err(|_| Error::Protocol("relay challenge entropy unavailable"))?;
    wire::send(ws, &Wire::Challenge(challenge), limits).await?;
    let Wire::Auth {
        role,
        key,
        signature,
        target,
    } = wire::recv(ws, limits).await?
    else {
        return Err(Error::AuthenticationFailed);
    };
    VerifyingKey::from_bytes(&key)
        .map_err(|_| Error::AuthenticationFailed)?
        .verify_strict(
            &wire::signed(&challenge, role, &target),
            &Signature::from_bytes(&signature),
        )
        .map_err(|_| Error::AuthenticationFailed)?;
    let (role, target) = if role == wire::REGISTER && target.is_empty() {
        (RelayRole::Host, None)
    } else if role == wire::CONNECT {
        (
            RelayRole::Client,
            Some(DeviceId::parse(&target).map_err(|_| Error::AuthenticationFailed)?),
        )
    } else {
        return Err(Error::AuthenticationFailed);
    };
    Ok(RelayProof {
        public_key: key,
        challenge,
        signature,
        role,
        target,
    })
}

pub(super) fn permitted(policy: &dyn RelayAdmission, proof: &RelayProof) -> bool {
    let id = DeviceId::from_public_key(&proof.public_key);
    match proof.role {
        RelayRole::Host => policy.admit_host(&id, proof),
        RelayRole::Client => proof.target.as_ref().is_some_and(|target| {
            policy.admit_client(&id, proof) && policy.may_connect(&id, target, proof)
        }),
    }
}
