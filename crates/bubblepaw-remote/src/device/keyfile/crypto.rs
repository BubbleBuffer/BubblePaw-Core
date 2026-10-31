use super::*;
use aes_gcm::Aes256Gcm;
use aes_gcm::aead::KeyInit;
use argon2::{Algorithm, Argon2, Block, Params, Version};

/// Key derivation: Argon2id v1.3 over the UTF-8 passphrase, 64 MiB, 3 passes, 1 lane, 32 bytes.
pub(super) fn derive_kdf_key(
    passphrase: &str,
    salt: &[u8; SALT_BYTES],
) -> Result<Zeroizing<[u8; SEED_BYTES]>, DeviceKeyError> {
    let params = Params::new(KDF_MEMORY_KIB, KDF_PASSES, KDF_LANES, Some(SEED_BYTES))
        .map_err(|_| DeviceKeyError::Internal("argon2 parameters"))?;
    let mut key = Zeroizing::new([0u8; SEED_BYTES]);
    let mut memory = Zeroizing::new(vec![Block::default(); params.block_count()]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into_with_memory(passphrase.as_bytes(), salt, &mut key[..], &mut memory[..])
        .map_err(|_| DeviceKeyError::Internal("argon2 derivation"))?;
    Ok(key)
}

pub(super) fn aes_cipher(key: &[u8; SEED_BYTES]) -> Result<Aes256Gcm, DeviceKeyError> {
    Aes256Gcm::new_from_slice(key).map_err(|_| DeviceKeyError::Internal("aes-256-gcm key"))
}

pub(super) fn seal_seed(
    kdf_key: &[u8; SEED_BYTES],
    nonce: &[u8; NONCE_BYTES],
    aad: &[u8],
    seed: &[u8; SEED_BYTES],
) -> Result<Vec<u8>, DeviceKeyError> {
    aes_cipher(kdf_key)?
        .encrypt(
            &Nonce::from(*nonce),
            Payload {
                msg: &seed[..],
                aad,
            },
        )
        .map_err(|_| DeviceKeyError::Internal("aes-256-gcm seal"))
}

/// Authenticated metadata: canonical JSON of every header field except `private` and `aead.nonce`,
/// keeping `aead.alg` and the KDF salt. Field values are used exactly as stored.
pub(super) fn aad_bytes(
    id: &str,
    label: &str,
    created: &str,
    public: &[u8; 32],
    salt: &[u8; SALT_BYTES],
) -> Vec<u8> {
    let root = object([
        ("format", jstr(FORMAT)),
        ("version", Json::Int(1)),
        ("type", jstr(KEY_TYPE)),
        ("id", jstr(id)),
        ("label", jstr(label)),
        ("created", jstr(created)),
        ("public", jstr(&URL_SAFE_NO_PAD.encode(public))),
        (
            "kdf",
            object([
                ("alg", jstr(KDF_ALG)),
                ("m", Json::Int(i64::from(KDF_MEMORY_KIB))),
                ("t", Json::Int(i64::from(KDF_PASSES))),
                ("p", Json::Int(i64::from(KDF_LANES))),
                ("salt", jstr(&URL_SAFE_NO_PAD.encode(salt))),
            ]),
        ),
        ("aead", object([("alg", jstr(AEAD_ALG))])),
    ]);
    canonical(&root).into_bytes()
}

pub(super) fn fill_random<R: RngCore>(rng: &mut R, out: &mut [u8]) -> Result<(), DeviceKeyError> {
    rng.try_fill_bytes(out).map_err(|_| DeviceKeyError::Entropy)
}

/// Strict unpadded base64url: the alphabet is checked, then the bytes must re-encode to the
/// same text, which rejects padding and non-zero trailing bits.
pub(super) fn decode_bytes(
    value: &str,
    field: &'static str,
) -> Result<Zeroizing<Vec<u8>>, DeviceKeyError> {
    if !value.bytes().all(|b| BASE64URL.contains(&b)) {
        return Err(DeviceKeyError::InvalidEncoding(field));
    }
    let decoded = Zeroizing::new(
        URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| DeviceKeyError::InvalidEncoding(field))?,
    );
    if Zeroizing::new(URL_SAFE_NO_PAD.encode(decoded.as_slice())).as_str() != value {
        return Err(DeviceKeyError::InvalidEncoding(field));
    }
    Ok(decoded)
}

pub(super) fn decode_fixed<const N: usize>(
    value: &str,
    field: &'static str,
) -> Result<[u8; N], DeviceKeyError> {
    let decoded = decode_bytes(value, field)?;
    <[u8; N]>::try_from(decoded.as_slice()).map_err(|_| DeviceKeyError::InvalidLength(field))
}

pub(super) fn fixed_secret(
    bytes: &[u8],
    field: &'static str,
) -> Result<Zeroizing<[u8; SEED_BYTES]>, DeviceKeyError> {
    if bytes.len() != SEED_BYTES {
        return Err(DeviceKeyError::InvalidLength(field));
    }
    let mut out = Zeroizing::new([0u8; SEED_BYTES]);
    out.copy_from_slice(bytes);
    Ok(out)
}
