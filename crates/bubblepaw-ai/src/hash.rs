//! Hex SHA-256 helpers.
use sha2::{Digest, Sha256};

pub fn fingerprint(bytes: &[u8]) -> String {
    hex_lower(Sha256::digest(bytes))
}

/// Lowercase hex of a digest or byte slice (sha2 0.11 digests no longer implement `LowerHex`).
pub fn hex_lower(bytes: impl AsRef<[u8]>) -> String {
    use std::fmt::Write;
    let bytes = bytes.as_ref();
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}
