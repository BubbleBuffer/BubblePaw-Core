use super::*;
use argon2::{Algorithm, Argon2, Block, Params, Version};
use sha2::{Digest, Sha256};

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A 50-bit secret. Debug intentionally omits the code; `code()` is explicit.
pub struct PairingCode {
    secret: Zeroizing<String>,
    pub(super) deadline: Instant,
    pub(super) used: bool,
}

impl PairingCode {
    pub fn generate() -> Result<Self> {
        Self::generate_at(Instant::now())
    }

    /// Caller-supplied monotonic issuance time also supports deterministic expiry tests.
    pub fn generate_at(issued: Instant) -> Result<Self> {
        let mut random = Zeroizing::new([0u8; 10]);
        OsRng
            .try_fill_bytes(&mut random[..])
            .map_err(|_| Error::Protocol("OS randomness unavailable"))?;
        let secret = random
            .iter()
            .map(|b| ALPHABET[usize::from(b & 31)] as char)
            .collect();
        let deadline = issued
            .checked_add(PAIR_TIMEOUT)
            .ok_or(Error::Protocol("invalid issuance time"))?;
        Ok(Self {
            secret: Zeroizing::new(secret),
            deadline,
            used: false,
        })
    }

    pub fn code(&self) -> &str {
        &self.secret
    }

    pub fn expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    pub fn used(&self) -> bool {
        self.used
    }
}

impl std::fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingCode")
            .field("used", &self.used)
            .field("expired", &self.expired())
            .finish_non_exhaustive()
    }
}

pub(super) fn normalize(input: &str) -> Result<Zeroizing<String>> {
    if input.len() != 10 {
        return Err(Error::WrongCode);
    }
    let normalized: String = input
        .bytes()
        .map(|b| match b.to_ascii_uppercase() {
            b'O' => '0',
            b'I' | b'L' => '1',
            b => b as char,
        })
        .collect();
    if !normalized.bytes().all(|b| ALPHABET.contains(&b)) {
        return Err(Error::WrongCode);
    }
    Ok(Zeroizing::new(normalized))
}

pub(super) async fn derive(
    code: Zeroizing<String>,
    host_id: DeviceId,
) -> Result<Zeroizing<[u8; 32]>> {
    tokio::task::spawn_blocking(move || {
        let params = Params::new(65_536, 3, 1, Some(32))
            .map_err(|_| Error::Protocol("invalid Argon2 parameters"))?;
        let mut memory = Zeroizing::new(vec![Block::default(); params.block_count()]);
        let mut psk = Zeroizing::new([0u8; 32]);
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into_with_memory(
                code.as_bytes(),
                host_id.to_string().as_bytes(),
                &mut psk[..],
                &mut memory[..],
            )
            .map_err(|_| Error::Protocol("pairing key derivation failed"))?;
        Ok(psk)
    })
    .await
    .map_err(|_| Error::Protocol("pairing key derivation task failed"))?
}

pub(super) fn comparison(handshake_hash: &[u8; 32]) -> String {
    let hash =
        Sha256::digest([b"bubblepaw-pair-comparison-v1".as_slice(), handshake_hash].concat());
    let number =
        u32::from_be_bytes(hash[..4].try_into().expect("SHA256 has four bytes")) % 1_000_000;
    format!("{number:06}")
}
