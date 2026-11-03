//! The two cleartext-free control messages of an egress session, carried inside
//! the encrypted tunnel before the byte stream starts.
use super::allow::Refusal;
use serde::{Deserialize, Serialize};

/// The requesting device's first message: which host it wants, and every host
/// its provider config names. The target must be one of the declared hosts.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Hello {
    pub v: u8,
    pub target: String,
    pub hosts: Vec<String>,
}

/// The forwarding host's answer: the byte stream follows, or a refusal.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Reply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Reply {
    pub(super) fn accepted() -> Self {
        Self {
            ok: true,
            code: None,
            reason: None,
        }
    }

    pub(super) fn refused(refusal: &Refusal) -> Self {
        Self {
            ok: false,
            code: Some(refusal.code.to_owned()),
            reason: Some(refusal.reason.clone()),
        }
    }
}
