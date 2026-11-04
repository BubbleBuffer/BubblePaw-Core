//! Provider API key controls, including on a host-less remote client.
use crate::{Client, ClientError};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Where the host obtains the configured provider's key. No value is returned.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKeySource {
    Stored,
    Environment,
    None,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProviderKeyStatus {
    pub is_set: bool,
    pub source: ProviderKeySource,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProviderKeyUpdate {
    #[serde(flatten)]
    pub status: ProviderKeyStatus,
    /// New requests use the accepted key; in-flight requests retain their key.
    pub applied: bool,
    /// A host without a live provider binding must restart to use the key.
    pub restart_needed: bool,
}

impl Client {
    /// Read presence and source only, without disclosing the key.
    pub async fn provider_key_status(&self, name: &str) -> Result<ProviderKeyStatus, ClientError> {
        let reply = self
            .call("providers.key.get", json!({"name": name}))
            .await?;
        serde_json::from_value(reply)
            .map_err(|_| ClientError::Protocol("Invalid provider key status".into()))
    }

    /// Store a key on the host. Requires the Full profile on remote connections.
    pub async fn set_provider_key(
        &self,
        name: &str,
        value: &str,
    ) -> Result<ProviderKeyUpdate, ClientError> {
        let reply = self
            .call("providers.key.set", json!({"name": name, "value": value}))
            .await?;
        serde_json::from_value(reply)
            .map_err(|_| ClientError::Protocol("Invalid provider key update".into()))
    }

    /// Remove the stored override, restoring the configured environment fallback.
    pub async fn remove_provider_key(&self, name: &str) -> Result<ProviderKeyUpdate, ClientError> {
        let reply = self
            .call("providers.key.remove", json!({"name": name}))
            .await?;
        serde_json::from_value(reply)
            .map_err(|_| ClientError::Protocol("Invalid provider key update".into()))
    }
}
