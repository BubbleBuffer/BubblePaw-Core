//! Device IDs: the first 80 bits of SHA-256 over the raw Ed25519 public key, written as 16
//! Crockford base32 characters in four hyphenated groups (`B7QK-2M9X-H4PD-R8TN`).
//!
//! Parsing and case folding are ASCII only, as in the host and the Go relay: characters such as
//! 'ß', 'ı', 'ſ', 'ﬁ' and 'é' are rejected, not folded to an ASCII letter.

use std::fmt;

use sha2::{Digest, Sha256};

use super::DeviceKeyError;

const DEVICE_ID_BYTES: usize = 10;
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The 80-bit device ID: the first 10 bytes of SHA-256 over the raw Ed25519 public key.
/// Displayed as 16 Crockford base32 characters in four hyphenated groups.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeviceId([u8; DEVICE_ID_BYTES]);

impl DeviceId {
    pub fn from_public_key(public: &[u8; 32]) -> Self {
        let digest = Sha256::digest(public);
        let mut id = [0u8; DEVICE_ID_BYTES];
        id.copy_from_slice(&digest[..DEVICE_ID_BYTES]);
        Self(id)
    }

    /// Accepts any letter case and ignores hyphens anywhere; O reads as 0, I and L as 1.
    pub fn parse(input: &str) -> Result<Self, DeviceKeyError> {
        let mut bits: u128 = 0;
        let mut count = 0usize;
        for ch in input.chars() {
            if ch == '-' {
                continue;
            }
            let symbol = match ch.to_ascii_uppercase() {
                'O' => '0',
                'I' | 'L' => '1',
                other => other,
            };
            let value = CROCKFORD
                .iter()
                .position(|&c| c as char == symbol)
                .ok_or(DeviceKeyError::InvalidId)?;
            count += 1;
            if count > 16 {
                return Err(DeviceKeyError::InvalidId);
            }
            bits = (bits << 5) | value as u128;
        }
        if count != 16 {
            return Err(DeviceKeyError::InvalidId);
        }
        let mut id = [0u8; DEVICE_ID_BYTES];
        id.copy_from_slice(&bits.to_be_bytes()[16 - DEVICE_ID_BYTES..]);
        Ok(Self(id))
    }

    pub fn as_bytes(&self) -> &[u8; DEVICE_ID_BYTES] {
        &self.0
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bits = self
            .0
            .iter()
            .fold(0u128, |acc, &byte| (acc << 8) | u128::from(byte));
        for index in 0..16u32 {
            if index > 0 && index % 4 == 0 {
                f.write_str("-")?;
            }
            let value = (bits >> (75 - 5 * index)) & 31;
            write!(f, "{}", CROCKFORD[value as usize] as char)?;
        }
        Ok(())
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({self})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use ed25519_dalek::SigningKey;
    use serde_json::Value;

    const FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/device-keys-contract.json"
    ));

    fn fixture() -> Value {
        serde_json::from_str(FIXTURE).expect("fixture parses")
    }

    fn unhex(value: &str) -> Vec<u8> {
        (0..value.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&value[i..i + 2], 16).expect("hex"))
            .collect()
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    fn seed_of(seed_hex: &str) -> [u8; 32] {
        unhex(seed_hex).try_into().expect("32-byte seed")
    }

    fn str_of(value: &Value) -> &str {
        value.as_str().expect("string")
    }

    #[test]
    fn ids_match_fixture_vectors_and_parse_aliases() {
        let fixture = fixture();
        for vector in fixture["ids"].as_array().expect("ids") {
            let seed = seed_of(str_of(&vector["seedHex"]));
            let public = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
            assert_eq!(b64(&public), str_of(&vector["public"]));
            assert_eq!(hex(&Sha256::digest(public)), str_of(&vector["sha256Hex"]));
            let id = DeviceId::from_public_key(&public);
            assert_eq!(id.to_string(), str_of(&vector["id"]));
            assert_eq!(id.as_bytes()[..], Sha256::digest(public)[..10]);
            assert_eq!(
                DeviceId::parse(str_of(&vector["id"])).expect("canonical"),
                id
            );
            assert_eq!(
                DeviceId::parse(&str_of(&vector["id"]).to_lowercase()).expect("lowercase"),
                id
            );
            assert_eq!(
                DeviceId::parse(&str_of(&vector["id"]).replace('-', "")).expect("unhyphenated"),
                id
            );
        }
        let alias = &fixture["ids"][2]["parse"];
        let parsed = DeviceId::parse(str_of(&alias["input"])).expect("aliases");
        assert_eq!(parsed.to_string(), str_of(&alias["canonical"]));
        assert_eq!(parsed.to_string(), str_of(&fixture["ids"][2]["id"]));
    }

    #[test]
    fn id_parse_rejects_wrong_length_and_foreign_symbols() {
        for input in [
            "",
            "B7QK2M9XH4PDR8T",
            "B7QK2M9XH4PDR8TNA",
            "B7QK2M9XH4PDR8TU",
            "B7QK2M9XH4PDR8T!",
            "B7QK 2M9XH4PDR8TN",
            "B7QKé2M9XH4PDR8T",
        ] {
            assert!(
                matches!(DeviceId::parse(input), Err(DeviceKeyError::InvalidId)),
                "accepted {input:?}"
            );
        }
    }

    #[test]
    fn id_parse_is_ascii_only_and_rejects_non_ascii_lookalikes() {
        // Go and JS fold some of these; we do not, and they are not Crockford digits.
        for symbol in ['ß', 'ı', 'ſ', 'ﬁ', 'é', 'İ', 'ᵽ'] {
            let single = symbol.to_string();
            assert!(
                matches!(DeviceId::parse(&single), Err(DeviceKeyError::InvalidId)),
                "accepted {single:?}"
            );
            // Inside an otherwise valid ID, both first and last.
            for position in [0usize, 15] {
                let mut chars: Vec<char> = "B7QK2M9XH4PDR8TN".chars().collect();
                chars[position] = symbol;
                let input: String = chars.into_iter().collect();
                assert!(
                    matches!(DeviceId::parse(&input), Err(DeviceKeyError::InvalidId)),
                    "accepted {input:?}"
                );
            }
            // Never accepted as an extra character either.
            let with_symbol = format!("B7QK{single}M9XH4PD8TN");
            assert!(
                matches!(
                    DeviceId::parse(&with_symbol),
                    Err(DeviceKeyError::InvalidId)
                ),
                "accepted {with_symbol:?}"
            );
        }
        // Sanity: the canonical form and the ASCII aliases still parse.
        assert!(DeviceId::parse("47Z3-3QX1-AJH6-2RKB").is_ok());
        assert!(DeviceId::parse("47z33qx1ajh62rkb").is_ok());
        assert_eq!(
            DeviceId::parse("hnobw2w6oqqilm47")
                .expect("aliases")
                .to_string(),
            "HN0B-W2W6-0QQ1-1M47"
        );
    }
}
