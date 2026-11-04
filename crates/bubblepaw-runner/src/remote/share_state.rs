//! Disposable remote-serving state, readable without owning the server.
use super::RemoteAddress;
use serde::{Deserialize, Serialize};
#[cfg(feature = "host")]
use std::sync::Weak;

/// Whether devices can reach the host now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteStatus {
    Connecting,
    Online,
    Offline,
}

/// A failure of the current sharing transport. Retry seconds are the actual
/// delay scheduled by the supervisor; exit codes exist only for exited children.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareFailure {
    pub transport: String,
    pub exit_code: Option<i32>,
    pub retry_after_secs: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareState {
    /// None when this host is not sharing.
    pub status: Option<RemoteStatus>,
    pub address: Option<RemoteAddress>,
    pub last_error: Option<String>,
    pub failure: Option<ShareFailure>,
}

#[cfg(feature = "host")]
#[derive(Clone, Default)]
pub struct ShareStateHandle(pub(super) Weak<super::server::Inner>);

#[cfg(feature = "host")]
impl ShareStateHandle {
    pub fn state(&self) -> ShareState {
        self.0
            .upgrade()
            .map_or_else(ShareState::default, |inner| inner.share_state())
    }
}

#[cfg(feature = "host")]
impl super::server::Inner {
    pub(super) fn share_state(&self) -> ShareState {
        ShareState {
            status: Some(*self.status.borrow()),
            address: self.address.borrow().clone(),
            last_error: self.last_error.lock().unwrap().clone(),
            failure: self.failure.lock().unwrap().clone(),
        }
    }
}
