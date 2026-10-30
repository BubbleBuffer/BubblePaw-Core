//! Caller-owned admission policy, independent of pairing and persistence.
use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};

use crate::{
    Error, Result,
    device::{DeviceId, noise_public_key},
};

pub use crate::store::Profile;

/// An Ed25519 public key, never a Noise/X25519 static key.
pub type PublicKey = [u8; 32];

/// The tunnel supplies an identity bound to its authenticated Noise static key.
/// Returning `None` denies admission. Implementations should return promptly;
/// blocking I/O and policy refresh belong outside this synchronous boundary.
pub trait PeerAuthorizer: Send + Sync {
    fn authorize(&self, id: &DeviceId, public_key: &PublicKey) -> Option<Profile>;
}

/// Local admission result. A profile is metadata for the host dispatcher to
/// enforce; the tunnel does not interpret or authorize application requests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedPeer {
    pub id: DeviceId,
    pub public_key: PublicKey,
    pub profile: Profile,
}

type DeviceList = BTreeMap<String, AuthorizedPeer>;

/// An atomically replaceable device list. Clones share the same live policy.
/// Validation happens before the write lock; an invalid replacement leaves the
/// entire previous policy in place. Changes apply to new tunnel admissions.
/// Already admitted tunnels retain their recorded grant; dispatchers that need
/// immediate revocation must recheck their policy for each request.
#[derive(Clone)]
pub struct DeviceListAuthorizer {
    devices: Arc<RwLock<Arc<DeviceList>>>,
}

impl DeviceListAuthorizer {
    pub fn new(peers: impl IntoIterator<Item = (DeviceId, PublicKey, Profile)>) -> Result<Self> {
        Ok(Self {
            devices: Arc::new(RwLock::new(Arc::new(validated(peers)?))),
        })
    }

    pub fn replace(
        &self,
        peers: impl IntoIterator<Item = (DeviceId, PublicKey, Profile)>,
    ) -> Result<()> {
        let devices = Arc::new(validated(peers)?);
        let previous = std::mem::replace(
            &mut *self
                .devices
                .write()
                .map_err(|_| Error::Protocol("device list lock is poisoned"))?,
            devices,
        );
        drop(previous);
        Ok(())
    }
}

fn validated(
    peers: impl IntoIterator<Item = (DeviceId, PublicKey, Profile)>,
) -> Result<DeviceList> {
    let mut devices = BTreeMap::new();
    for (id, public_key, profile) in peers {
        noise_public_key(&public_key)?;
        if DeviceId::from_public_key(&public_key) != id {
            return Err(Error::Protocol(
                "device list ID does not match its public key",
            ));
        }
        if devices
            .insert(
                id.to_string(),
                AuthorizedPeer {
                    id,
                    public_key,
                    profile,
                },
            )
            .is_some()
        {
            return Err(Error::Protocol("device list contains a duplicate ID"));
        }
    }
    Ok(devices)
}

impl PeerAuthorizer for DeviceListAuthorizer {
    fn authorize(&self, id: &DeviceId, public_key: &PublicKey) -> Option<Profile> {
        let snapshot = self.devices.read().ok()?.clone();
        snapshot
            .get(&id.to_string())
            .filter(|peer| peer.public_key == *public_key)
            .map(|peer| peer.profile)
    }
}

impl<A: PeerAuthorizer + ?Sized> PeerAuthorizer for Arc<A> {
    fn authorize(&self, id: &DeviceId, public_key: &PublicKey) -> Option<Profile> {
        self.as_ref().authorize(id, public_key)
    }
}

/// Compatibility convenience: an exact Ed25519 key allowlist grants `Full`.
/// Use a device list or custom policy when individual profiles matter.
impl PeerAuthorizer for [PublicKey] {
    fn authorize(&self, id: &DeviceId, public_key: &PublicKey) -> Option<Profile> {
        (DeviceId::from_public_key(public_key) == *id && self.contains(public_key))
            .then_some(Profile::Full)
    }
}

impl<const N: usize> PeerAuthorizer for [PublicKey; N] {
    fn authorize(&self, id: &DeviceId, public_key: &PublicKey) -> Option<Profile> {
        self.as_slice().authorize(id, public_key)
    }
}

impl PeerAuthorizer for Vec<PublicKey> {
    fn authorize(&self, id: &DeviceId, public_key: &PublicKey) -> Option<Profile> {
        self.as_slice().authorize(id, public_key)
    }
}
