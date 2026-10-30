//! Device identities and BubbleBuffer `.bpkey` files.
//!
//! Uses the BubbleBuffer device-key format, version 1.
//! Protected files authenticate the format name together with their metadata.

use std::fmt;

use curve25519_dalek::edwards::CompressedEdwardsY;
use ed25519_dalek::{Signer, SigningKey};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256, Sha512};
use zeroize::Zeroizing;

mod error;
mod ids;
mod keyfile;

pub use error::DeviceKeyError;
pub use ids::DeviceId;
pub use keyfile::KeyFile;

/// An Ed25519 device identity. Secret material is zeroized by `SigningKey` on drop.
pub struct DeviceKey(SigningKey);

impl DeviceKey {
    pub fn generate() -> Result<Self, DeviceKeyError> {
        let mut seed = Zeroizing::new([0; 32]);
        OsRng
            .try_fill_bytes(&mut seed[..])
            .map_err(|_| DeviceKeyError::Entropy)?;
        Ok(Self::from_seed(&seed))
    }

    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self(SigningKey::from_bytes(seed))
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.0.verifying_key().to_bytes()
    }

    pub fn id(&self) -> DeviceId {
        DeviceId::from_public_key(&self.public_key())
    }

    /// Full SHA-256 of the Ed25519 public key, as lowercase hexadecimal.
    pub fn fingerprint(&self) -> String {
        Sha256::digest(self.public_key())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.0.sign(message).to_bytes()
    }

    /// libsodium-compatible Ed25519 seed to X25519 scalar conversion.
    pub fn noise_private_key(&self) -> Zeroizing<[u8; 32]> {
        let seed = Zeroizing::new(self.0.to_bytes());
        let digest = Zeroizing::new(<[u8; 64]>::from(Sha512::digest(&seed[..])));
        let mut scalar = Zeroizing::new([0; 32]);
        scalar.copy_from_slice(&digest[..32]);
        scalar[0] &= 248;
        scalar[31] &= 127;
        scalar[31] |= 64;
        scalar
    }
}

impl From<SigningKey> for DeviceKey {
    fn from(signing: SigningKey) -> Self {
        Self(signing)
    }
}

impl fmt::Debug for DeviceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceKey")
            .field("id", &self.id())
            .finish_non_exhaustive()
    }
}

/// libsodium-compatible Ed25519 public key to X25519 conversion.
///
/// Decompression alone accepts noncanonical encodings and points with torsion;
/// require canonical encoding and a nonidentity point in the prime-order subgroup.
pub fn noise_public_key(public: &[u8; 32]) -> Result<[u8; 32], DeviceKeyError> {
    let point = CompressedEdwardsY(*public)
        .decompress()
        .ok_or(DeviceKeyError::InvalidPublicKey)?;
    if point.compress().to_bytes() != *public || point.is_small_order() || !point.is_torsion_free()
    {
        return Err(DeviceKeyError::InvalidPublicKey);
    }
    Ok(point.to_montgomery().to_bytes())
}
