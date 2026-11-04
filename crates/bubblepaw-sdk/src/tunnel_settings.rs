//! Transport-independent host settings. These calls also work on clients built
//! without the `host` feature. The host refuses executable selection remotely.
use crate::{Client, ClientError};
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};

pub use bubblepaw_runner::remote::{RemoteStatus, ShareFailure, ShareState};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct SignOutResult {
    pub changed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct CloudflaredInfo {
    pub path: PathBuf,
    pub version: Option<String>,
    pub pinned: String,
    pub update_available: bool,
    pub managed: bool,
    pub chosen: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct CloudflaredResult {
    pub info: Option<CloudflaredInfo>,
}

impl Client {
    /// Forget the host's browser sign-in certificate only. Requires Full.
    pub async fn cloudflare_sign_out(&self) -> Result<SignOutResult, ClientError> {
        decode(self.call("remote.cloudflare.sign_out", json!({})).await?)
    }

    pub async fn cloudflared_info(&self) -> Result<CloudflaredResult, ClientError> {
        decode(self.call("remote.cloudflared.info", json!({})).await?)
    }

    /// Validate and persist the host user's executable choice. None restores
    /// managed-copy/PATH discovery. Local Full callers only; restart sharing
    /// to run the selected executable.
    pub async fn set_cloudflared(
        &self,
        path: Option<&Path>,
    ) -> Result<CloudflaredResult, ClientError> {
        decode(
            self.call("remote.cloudflared.set", json!({"path":path}))
                .await?,
        )
    }

    pub async fn share_state(&self) -> Result<ShareState, ClientError> {
        decode(self.call("remote.share.state", json!({})).await?)
    }
}

fn decode<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<T, ClientError> {
    serde_json::from_value(value).map_err(|error| ClientError::Protocol(error.to_string()))
}
