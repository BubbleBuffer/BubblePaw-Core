use std::fmt;

/// Failures are distinct: `AuthenticationFailed` means the AEAD tag did not verify (wrong
/// passphrase or altered authenticated metadata); the other variants are validation failures
/// found before or after decryption.
#[derive(Debug)]
pub enum DeviceKeyError {
    InvalidId,
    /// The Ed25519 key is malformed, noncanonical or outside the prime-order subgroup.
    InvalidPublicKey,
    /// A proof role other than the wire strings `host` and `viewer`.
    UnknownRole,
    /// JSON syntax, duplicate keys, unsupported JSON value kinds or nesting depth.
    Malformed(&'static str),
    MissingField(&'static str),
    UnknownField(String),
    /// Wrong JSON type, or a value outside what the format allows.
    InvalidField(&'static str),
    InvalidEncoding(&'static str),
    InvalidLength(&'static str),
    UnsupportedFormat,
    UnsupportedVersion,
    UnsupportedType,
    UnsupportedKdf,
    UnsupportedAead,
    /// `kdf` and `aead` must both be null (plain) or both be objects (protected).
    IncompleteProtection,
    IdMismatch,
    PublicKeyMismatch,
    PassphraseRequired,
    PassphraseNotAccepted,
    AuthenticationFailed,
    Entropy,
    /// The proof hash did not verify under the supplied key, or the fields do not match the
    /// signature that was made over them.
    InvalidSignature,
    Internal(&'static str),
}

impl fmt::Display for DeviceKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPublicKey => write!(f, "invalid Ed25519 public key for Noise conversion"),
            Self::InvalidId => write!(f, "device ID must be 16 Crockford base32 characters"),
            Self::UnknownRole => write!(
                f,
                "unknown role (only \"host\" and \"viewer\" are accepted)"
            ),
            Self::Malformed(reason) => write!(f, "malformed key file JSON: {reason}"),
            Self::MissingField(field) => write!(f, "key file is missing required field `{field}`"),
            Self::UnknownField(field) => write!(f, "key file has unknown field `{field}`"),
            Self::InvalidField(field) => {
                write!(f, "key file field `{field}` has an invalid type or value")
            }
            Self::InvalidEncoding(field) => write!(
                f,
                "key file field `{field}` is not canonical unpadded base64url"
            ),
            Self::InvalidLength(field) => {
                write!(f, "key file field `{field}` has the wrong length")
            }
            Self::UnsupportedFormat => {
                write!(f, "not a device key file (unknown format)")
            }
            Self::UnsupportedVersion => write!(
                f,
                "unsupported device key file version (this build reads version 1)"
            ),
            Self::UnsupportedType => {
                write!(f, "unsupported device key type (this build reads ed25519)")
            }
            Self::UnsupportedKdf => write!(
                f,
                "unsupported key derivation (only argon2id, m=65536, t=3, p=1)"
            ),
            Self::UnsupportedAead => write!(f, "unsupported cipher (only aes-256-gcm)"),
            Self::IncompleteProtection => write!(
                f,
                "key file protection is incomplete: kdf and aead must both be null or both present"
            ),
            Self::IdMismatch => write!(f, "device ID does not match the public key"),
            Self::PublicKeyMismatch => write!(f, "public key does not match the private seed"),
            Self::PassphraseRequired => write!(f, "this device key is passphrase-protected"),
            Self::PassphraseNotAccepted => write!(f, "this device key is not passphrase-protected"),
            Self::AuthenticationFailed => {
                write!(f, "wrong passphrase or the key file was modified")
            }
            Self::Entropy => write!(f, "operating system randomness is unavailable"),
            Self::InvalidSignature => write!(f, "relay proof signature did not verify"),
            Self::Internal(what) => write!(f, "internal cryptography failure: {what}"),
        }
    }
}

impl std::error::Error for DeviceKeyError {}
