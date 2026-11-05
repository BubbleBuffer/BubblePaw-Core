//! Key operations share provider/config update locks, without editing config.
use super::{ApiError, ApiResult, ProviderSetup, bad, io_error, valid_name};
use crate::{config::EffectiveConfig, provider_keys};
use anyhow::Result;
use reqwest::StatusCode;
use serde_json::{Value, json};

impl ProviderSetup {
    fn key_config(&self, name: &str) -> ApiResult<EffectiveConfig> {
        if !valid_name(name) {
            return Err(bad());
        }
        let effective = match &self.0.live {
            Some(live) => live.snapshot().effective.clone(),
            None => self.0.load()?,
        };
        let settings = effective.settings().map_err(|_| bad())?;
        let provider = settings
            .providers
            .get(name)
            .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "Provider is not configured".into()))?;
        if matches!(provider.kind.as_deref(), Some("codex" | "scripted")) {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "This provider does not use a provider API key".into(),
            ));
        }
        Ok(effective)
    }

    /// Read presence and source only. The key is never part of a response.
    pub fn key_status(&self, name: &str) -> Result<Value> {
        let _files = self.0.files.lock().map_err(|_| io_error())?;
        let effective = self.key_config(name)?;
        Ok(key_status(&effective, name)?)
    }

    /// Persist a key, or remove the stored override and fall back to key_env.
    /// The existing provider generation boundary supplies next-request uptake.
    pub async fn set_key(&self, name: &str, value: Option<&str>) -> Result<Value> {
        Ok(self.set_key_inner(name, value).await?)
    }

    async fn set_key_inner(&self, name: &str, value: Option<&str>) -> ApiResult<Value> {
        if let Some(value) = value {
            provider_keys::validate(value).map_err(|_| bad())?;
        }
        let _host = match &self.0.host_config {
            Some(host) => Some(host.lock_update().await),
            None => None,
        };
        let _files = self.0.files.lock().map_err(|_| io_error())?;
        let _providers = self
            .0
            .live
            .as_ref()
            .map(|live| live.lock_update())
            .transpose()
            .map_err(|_| io_error())?;
        let effective = self.key_config(name)?;
        let settings = effective.settings().map_err(|_| bad())?;
        let dir = &settings.storage.data_dir;
        let old = provider_keys::read(dir, name).map_err(|_| io_error())?;
        match value {
            Some(value) => provider_keys::write(dir, name, value),
            None => provider_keys::remove(dir, name),
        }
        .map_err(|_| io_error())?;
        if let Some(live) = &self.0.live {
            let prepared = match live.prepare_credentials() {
                Ok(prepared) => prepared,
                Err(_) => {
                    let restored = match &old {
                        Some(old) => provider_keys::write(dir, name, old),
                        None => provider_keys::remove(dir, name),
                    };
                    return Err(ApiError(
                        StatusCode::CONFLICT,
                        if restored.is_ok() {
                            "Cannot apply provider key; previous stored key restored"
                        } else {
                            "Cannot apply provider key; stored key restoration failed"
                        }
                        .into(),
                    ));
                }
            };
            live.apply_prepared(prepared);
        }
        let mut status = key_status(&effective, name)?;
        status["applied"] = json!(self.0.live.is_some());
        status["restart_needed"] = json!(self.0.live.is_none());
        Ok(status)
    }
}

fn key_status(effective: &EffectiveConfig, name: &str) -> ApiResult<Value> {
    let settings = effective.settings().map_err(|_| bad())?;
    let source = if provider_keys::read(&settings.storage.data_dir, name)
        .map_err(|_| io_error())?
        .is_some()
    {
        "stored"
    } else if settings
        .providers
        .get(name)
        .and_then(|provider| provider.key_env.as_ref())
        .and_then(|name| effective.environment(name))
        .is_some_and(|value| !value.is_empty())
    {
        "environment"
    } else {
        "none"
    };
    Ok(json!({"source": source, "is_set": source != "none"}))
}
