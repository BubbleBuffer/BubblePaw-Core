//! Workspace trust over a host client: list, grant and revoke the user's trusted
//! workspaces. Local Full callers only; a remote device is refused. A grant or
//! revoke reloads the host's config, so sessions opened afterwards see it.
use crate::{Client, ClientError};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

pub use bubblepaw_runner::workspace_trust::TrustRecord;

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct TrustList {
    pub workspaces: Vec<TrustRecord>,
    /// The host's own workspace: `status`, `workspace`, `config_file`, `sections`, `how_to_trust`.
    #[serde(default)]
    pub workspace: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct TrustGrant {
    pub trusted: TrustRecord,
    /// The config reload report, when the host reloads live.
    #[serde(default)]
    pub reload: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct TrustRevoke {
    pub removed: bool,
    #[serde(default)]
    pub reload: Option<Value>,
}

impl Client {
    /// `workspace_trust.list`: the recorded trusted workspaces.
    pub async fn trusted_workspaces(&self) -> Result<TrustList, ClientError> {
        decode(self.call("workspace_trust.list", json!({})).await?)
    }

    /// `workspace_trust.trust`: trust a workspace folder for sessions started afterwards.
    pub async fn trust_workspace(&self, path: &Path) -> Result<TrustGrant, ClientError> {
        decode(
            self.call("workspace_trust.trust", json!({"path": path}))
                .await?,
        )
    }

    /// `workspace_trust.revoke`: remove a workspace's trust record.
    pub async fn revoke_workspace_trust(&self, path: &Path) -> Result<TrustRevoke, ClientError> {
        decode(
            self.call("workspace_trust.revoke", json!({"path": path}))
                .await?,
        )
    }
}

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, ClientError> {
    serde_json::from_value(value).map_err(|error| ClientError::Protocol(error.to_string()))
}
