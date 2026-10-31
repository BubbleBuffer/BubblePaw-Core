//! Shared device key file (`.bpkey`), format version 1: Ed25519 seed, optional
//! Argon2id + AES-256-GCM protection, and a strict JSON reader.
//!
//! Format and encodings: the repository's `docs/remote.md`.
//! The crate's `tests/fixtures/device-keys-contract.json` pins the bytes; tests check it.
//!
//! Nothing here reads a clock, the network or storage. Callers supply the label and the created
//! timestamp and decide where the file lives. Unlocked seeds are never cached on a `KeyFile`.

use aes_gcm::Nonce;
use aes_gcm::aead::{Aead, Payload};
use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{SigningKey, VerifyingKey};

use rand_core::OsRng;

use rand_core::RngCore;
use zeroize::{Zeroize, Zeroizing};

use super::{DeviceId, DeviceKeyError};

mod crypto;
mod json;
mod validation;
use crypto::*;
use json::*;
use validation::*;

const FORMAT: &str = "bubblebuffer-device-key";
const KEY_TYPE: &str = "ed25519";
const KDF_ALG: &str = "argon2id";
const AEAD_ALG: &str = "aes-256-gcm";
/// Argon2id parameters fixed by format version 1: 64 MiB, 3 passes, 1 lane, Argon2 v1.3.
const KDF_MEMORY_KIB: u32 = 65_536;
const KDF_PASSES: u32 = 3;
const KDF_LANES: u32 = 1;
const SEED_BYTES: usize = 32;
const SALT_BYTES: usize = 16;
const NONCE_BYTES: usize = 12;
/// Encrypted seed: ciphertext plus the 16-byte AES-GCM tag.
const SEALED_SEED_BYTES: usize = SEED_BYTES + 16;
const MAX_JSON_DEPTH: usize = 4;
const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// A parsed or created key file. Immutable; the seed is only ever unlocked into a caller-owned
/// `SigningKey`. A plain file stores the seed itself, which is why `to_json` on one is secret.
pub struct KeyFile {
    id: DeviceId,
    // Preserve imported spelling: JCS authenticates string values without normalizing IDs.
    id_text: String,
    label: String,
    created: String,
    public: [u8; 32],
    protection: Protection,
}

enum Protection {
    Plain {
        seed: Zeroizing<[u8; SEED_BYTES]>,
    },
    Protected {
        salt: [u8; SALT_BYTES],
        nonce: [u8; NONCE_BYTES],
        /// Ciphertext and tag of the seed, 48 bytes.
        sealed: Vec<u8>,
    },
}

impl KeyFile {
    /// Wraps an existing identity, optionally protecting it with fresh salt and nonce.
    pub fn from_device(
        key: &super::DeviceKey,
        label: &str,
        created: &str,
        passphrase: Option<&str>,
    ) -> Result<Self, DeviceKeyError> {
        Self::from_seed(
            &mut OsRng,
            label,
            created,
            Zeroizing::new(key.0.to_bytes()),
            passphrase,
        )
    }

    /// Creates a new key from OS randomness. Without a passphrase the file stores the seed.
    pub fn create(
        label: &str,
        created: &str,
        passphrase: Option<&str>,
    ) -> Result<Self, DeviceKeyError> {
        Self::create_with_rng(&mut OsRng, label, created, passphrase)
    }

    /// Parses and validates a key file. Protected files get structural and public-to-ID checks
    /// here; `unlock` adds decryption and seed-to-public-to-ID validation.
    pub fn parse(text: &str) -> Result<Self, DeviceKeyError> {
        let mut entries = match parse_document(text)? {
            Json::Object(entries) => entries,
            _ => return Err(DeviceKeyError::Malformed("key file must be a JSON object")),
        };
        if take_str(&mut entries, "format", "format")?.as_str() != FORMAT {
            return Err(DeviceKeyError::UnsupportedFormat);
        }
        match take(&mut entries, "version", "version")? {
            Json::Int(1) => {}
            Json::Int(_) => return Err(DeviceKeyError::UnsupportedVersion),
            _ => return Err(DeviceKeyError::InvalidField("version")),
        }
        if take_str(&mut entries, "type", "type")?.as_str() != KEY_TYPE {
            return Err(DeviceKeyError::UnsupportedType);
        }
        let id_text = take_str(&mut entries, "id", "id")?;
        let label = take_str(&mut entries, "label", "label")?;
        let created = take_str(&mut entries, "created", "created")?;
        let public = decode_fixed::<32>(&take_str(&mut entries, "public", "public")?, "public")?;
        VerifyingKey::from_bytes(&public).map_err(|_| DeviceKeyError::InvalidEncoding("public"))?;

        let kdf = take(&mut entries, "kdf", "kdf")?;
        let aead = take(&mut entries, "aead", "aead")?;
        let mut private = take_str(&mut entries, "private", "private")?;
        let decoded = decode_bytes(&private, "private");
        private.zeroize();
        let private_bytes = decoded?;
        let protection = match (kdf, aead) {
            (Json::Null, Json::Null) => Protection::Plain {
                seed: fixed_secret(&private_bytes, "private")?,
            },
            (Json::Object(kdf), Json::Object(aead)) => {
                let salt = parse_kdf(kdf)?;
                let nonce = parse_aead(aead)?;
                if private_bytes.len() != SEALED_SEED_BYTES {
                    return Err(DeviceKeyError::InvalidLength("private"));
                }
                Protection::Protected {
                    salt,
                    nonce,
                    sealed: private_bytes.to_vec(),
                }
            }
            _ => return Err(DeviceKeyError::IncompleteProtection),
        };

        let id = DeviceId::parse(&id_text)?;
        reject_unknown(&entries, "")?;
        if let Protection::Plain { seed } = &protection
            && SigningKey::from_bytes(seed).verifying_key().to_bytes() != public
        {
            return Err(DeviceKeyError::PublicKeyMismatch);
        }
        if id != DeviceId::from_public_key(&public) {
            return Err(DeviceKeyError::IdMismatch);
        }
        Ok(Self {
            id,
            id_text: id_text.to_string(),
            label: label.to_string(),
            created: created.to_string(),
            public,
            protection,
        })
    }

    /// Unlocks directly into the host-facing identity wrapper.
    pub fn unlock_device(
        &self,
        passphrase: Option<&str>,
    ) -> Result<super::DeviceKey, DeviceKeyError> {
        self.unlock(passphrase).map(super::DeviceKey::from)
    }

    /// Recovers the signing key. Plain files take `None`; protected files take the passphrase.
    /// A wrong passphrase or altered metadata yields `AuthenticationFailed`.
    pub fn unlock(&self, passphrase: Option<&str>) -> Result<SigningKey, DeviceKeyError> {
        let seed = self.open_seed(passphrase)?;
        let signing = SigningKey::from_bytes(&seed);
        if signing.verifying_key().to_bytes() != self.public {
            return Err(DeviceKeyError::PublicKeyMismatch);
        }
        if DeviceId::from_public_key(&self.public) != self.id {
            return Err(DeviceKeyError::IdMismatch);
        }
        Ok(signing)
    }

    /// Unlocks with the current passphrase, then returns the same identity protected by
    /// `new_passphrase` (or plain when `None`). Protected output always has a fresh salt and nonce.
    pub fn rewrap(
        &self,
        current_passphrase: Option<&str>,
        new_passphrase: Option<&str>,
    ) -> Result<Self, DeviceKeyError> {
        self.rewrap_with_rng(&mut OsRng, current_passphrase, new_passphrase)
    }

    /// Stable serialization: fixed field order, two-space indentation, no trailing newline.
    /// The output contains `private` as stored: the seed for plain files.
    pub fn to_json(&self) -> Result<String, DeviceKeyError> {
        let (kdf, aead, private) = match &self.protection {
            Protection::Plain { seed } => {
                (Json::Null, Json::Null, URL_SAFE_NO_PAD.encode(&seed[..]))
            }
            Protection::Protected {
                salt,
                nonce,
                sealed,
            } => (
                object([
                    ("alg", jstr(KDF_ALG)),
                    ("m", Json::Int(i64::from(KDF_MEMORY_KIB))),
                    ("t", Json::Int(i64::from(KDF_PASSES))),
                    ("p", Json::Int(i64::from(KDF_LANES))),
                    ("salt", jstr(&URL_SAFE_NO_PAD.encode(salt))),
                ]),
                object([
                    ("alg", jstr(AEAD_ALG)),
                    ("nonce", jstr(&URL_SAFE_NO_PAD.encode(nonce))),
                ]),
                URL_SAFE_NO_PAD.encode(sealed),
            ),
        };
        let private = Zeroizing::new(private);
        let root = object([
            ("format", jstr(FORMAT)),
            ("version", Json::Int(1)),
            ("type", jstr(KEY_TYPE)),
            ("id", jstr(&self.id_text)),
            ("label", jstr(&self.label)),
            ("created", jstr(&self.created)),
            ("public", jstr(&URL_SAFE_NO_PAD.encode(self.public))),
            ("kdf", kdf),
            ("aead", aead),
            ("private", jstr(&private)),
        ]);
        let mut out = String::new();
        write_pretty(&root, 0, &mut out);
        Ok(out)
    }

    pub fn id(&self) -> DeviceId {
        self.id
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn created(&self) -> &str {
        &self.created
    }

    pub fn public_key(&self) -> &[u8; 32] {
        &self.public
    }

    pub fn is_protected(&self) -> bool {
        matches!(self.protection, Protection::Protected { .. })
    }

    fn create_with_rng<R: RngCore>(
        rng: &mut R,
        label: &str,
        created: &str,
        passphrase: Option<&str>,
    ) -> Result<Self, DeviceKeyError> {
        let mut seed = Zeroizing::new([0u8; SEED_BYTES]);
        fill_random(rng, &mut seed[..])?;
        Self::from_seed(rng, label, created, seed, passphrase)
    }

    fn rewrap_with_rng<R: RngCore>(
        &self,
        rng: &mut R,
        current_passphrase: Option<&str>,
        new_passphrase: Option<&str>,
    ) -> Result<Self, DeviceKeyError> {
        let signing = self.unlock(current_passphrase)?;
        let seed = Zeroizing::new(signing.to_bytes());
        drop(signing);
        Self::from_seed(rng, &self.label, &self.created, seed, new_passphrase)
    }

    /// Builds a file for `seed` with fresh protection. The seed is only read; the caller zeroizes it.
    fn from_seed<R: RngCore>(
        rng: &mut R,
        label: &str,
        created: &str,
        seed: Zeroizing<[u8; SEED_BYTES]>,
        passphrase: Option<&str>,
    ) -> Result<Self, DeviceKeyError> {
        let public = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
        let id = DeviceId::from_public_key(&public);
        let id_text = id.to_string();
        let protection = match passphrase {
            None => Protection::Plain { seed },
            Some(passphrase) => {
                let mut salt = [0u8; SALT_BYTES];
                fill_random(rng, &mut salt)?;
                let mut nonce = [0u8; NONCE_BYTES];
                fill_random(rng, &mut nonce)?;
                let kdf_key = derive_kdf_key(passphrase, &salt)?;
                let aad = aad_bytes(&id_text, label, created, &public, &salt);
                let sealed = seal_seed(&kdf_key, &nonce, &aad, &seed)?;
                Protection::Protected {
                    salt,
                    nonce,
                    sealed,
                }
            }
        };
        Ok(Self {
            id,
            id_text,
            label: label.to_owned(),
            created: created.to_owned(),
            public,
            protection,
        })
    }

    /// Returns the decrypted seed for a plain or protected file. Only the passphrase is checked
    /// here; the caller validates the seed against the public key and ID.
    fn open_seed(
        &self,
        passphrase: Option<&str>,
    ) -> Result<Zeroizing<[u8; SEED_BYTES]>, DeviceKeyError> {
        match (&self.protection, passphrase) {
            (Protection::Plain { seed }, None) => Ok(seed.clone()),
            (Protection::Plain { .. }, Some(_)) => Err(DeviceKeyError::PassphraseNotAccepted),
            (Protection::Protected { .. }, None) => Err(DeviceKeyError::PassphraseRequired),
            (
                Protection::Protected {
                    salt,
                    nonce,
                    sealed,
                },
                Some(passphrase),
            ) => {
                let kdf_key = derive_kdf_key(passphrase, salt)?;
                let aad = aad_bytes(
                    &self.id_text,
                    &self.label,
                    &self.created,
                    &self.public,
                    salt,
                );
                let cipher = aes_cipher(&kdf_key)?;
                let plain = Zeroizing::new(
                    cipher
                        .decrypt(
                            &Nonce::from(*nonce),
                            Payload {
                                msg: sealed,
                                aad: &aad,
                            },
                        )
                        .map_err(|_| DeviceKeyError::AuthenticationFailed)?,
                );
                fixed_secret(&plain, "private")
            }
        }
    }
}

#[doc(hidden)]
impl KeyFile {
    /// The authenticated metadata (AAD) this file is sealed with, or None for a plain file.
    /// Exposed only so the contract tests can compare it with the shared fixture.
    pub fn aad_for_contract(&self) -> Option<Vec<u8>> {
        match &self.protection {
            Protection::Plain { .. } => None,
            Protection::Protected { salt, .. } => Some(aad_bytes(
                &self.id_text,
                &self.label,
                &self.created,
                &self.public,
                salt,
            )),
        }
    }
}

impl fmt::Debug for KeyFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyFile")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("created", &self.created)
            .field("protected", &self.is_protected())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod contract_generator;
