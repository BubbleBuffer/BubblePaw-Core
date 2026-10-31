//! Saving, removing and reloading provider configuration. Every host takes the
//! same locks and runs the same checks; each transport only maps the result.
use super::input::{ProviderInput, provider_table, validate_setup};
use super::{
    ApiError, ApiResult, ProviderSetup, bad, conflict, io_error, provider_json, table,
    target_provider, valid_name,
};
use anyhow::Result;
use reqwest::StatusCode;
use serde_json::{Value, json};
use toml_edit::Item;

impl ProviderSetup {
    /// Write and live-apply one provider. `input` is the provider JSON: `kind`,
    /// optional `base_url`, `key_env`, `auth_file`, `reasoning`, and `models`.
    pub async fn save(&self, name: &str, input: Value) -> Result<Value> {
        Ok(self.save_inner(name, input).await?)
    }
    /// Remove a provider from the target file and live-apply the result.
    pub async fn remove(&self, name: &str) -> Result<Value> {
        Ok(self.remove_inner(name).await?)
    }
    /// Reload from the original layers. An unapplied reload is still `Ok`: the
    /// payload carries `applied: false`, the diff and an `error` summary.
    pub async fn reload(&self) -> Result<Value> {
        Ok(self.reload_inner().await?)
    }
    async fn save_inner(&self, name: &str, input: Value) -> ApiResult<Value> {
        let _host_update = match &self.0.host_config {
            Some(host) => Some(host.lock_update().await),
            None => None,
        };
        let reply = self.save_locked(name, input)?;
        self.0.apply_host().await?;
        Ok(reply)
    }
    fn save_locked(&self, name: &str, input: Value) -> ApiResult<Value> {
        let provider = serde_json::from_value::<ProviderInput>(input)
            .map_err(|_| bad())?
            .settings(name, &self.0.target)
            .map_err(|_| bad())?;
        let _guard = self.0.files.lock().map_err(|_| io_error())?;
        let _update = self.0.provider_update_guard()?;
        let settings = self.0.load()?.settings().map_err(|_| bad())?;
        validate_setup(&provider, &settings.web.provider_key_envs)?;
        let mut doc = self.0.document()?;
        let lower = self.0.lower()?;
        for (field, clearing) in [
            ("base_url", provider.base_url.is_none()),
            ("key_env", provider.key_env.is_none()),
            ("auth_file", provider.auth_file.is_none()),
        ] {
            if clearing && lower.value(&format!("providers.{name}.{field}")).is_some() {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "{field} is inherited from defaults and can't be cleared here; set a value instead"
                    ),
                ));
            }
        }
        let previous = doc.get("providers").and_then(|item| item.get(name));
        let provider_item = provider_table(&provider, previous)?;
        table(&mut doc, "providers")?.insert(name, provider_item);
        let candidate = self.0.candidate(&doc)?;
        let settings = candidate.settings().map_err(|_| bad())?;
        if settings.providers.get(name).map(provider_json).as_ref()
            != Some(&provider_json(&provider))
        {
            return Err(conflict());
        }
        provider.validate(name).map_err(|_| bad())?;
        self.0.write_apply(&doc, candidate)
    }
    async fn remove_inner(&self, name: &str) -> ApiResult<Value> {
        let _host_update = match &self.0.host_config {
            Some(host) => Some(host.lock_update().await),
            None => None,
        };
        let reply = self.remove_locked(name)?;
        self.0.apply_host().await?;
        Ok(reply)
    }
    fn remove_locked(&self, name: &str) -> ApiResult<Value> {
        if !valid_name(name) {
            return Err(bad());
        }
        let _guard = self.0.files.lock().map_err(|_| io_error())?;
        let _update = self.0.provider_update_guard()?;
        let mut doc = self.0.document()?;
        if !target_provider(&doc, name) {
            return Err(ApiError(
                StatusCode::NOT_FOUND,
                "Provider is not in the target file".into(),
            ));
        }
        let lower = self.0.lower()?;
        let target_model = doc.get("model");
        let target_main_provider = target_model
            .and_then(|item| item.get("main"))
            .and_then(Item::as_str)
            .and_then(|model| {
                model
                    .split_once('/')
                    .map(|(provider, _)| provider)
                    .or_else(|| {
                        target_model
                            .and_then(|item| item.get("provider"))
                            .and_then(Item::as_str)
                            .or_else(|| lower.value("model.provider").and_then(toml::Value::as_str))
                    })
            });
        if target_main_provider.is_some_and(|provider| provider == name) {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "Provider is selected by model.main in the target file".into(),
            ));
        }
        if let Some(removed) = table(&mut doc, "providers")?.remove(name)
            && let Some(removed) = removed.as_table()
        {
            // Header comments belong to the table's decoration in toml_edit.
            // Retain them even when removing the final table from the document.
            let mut trailing = doc.trailing().as_str().unwrap_or_default().to_owned();
            for decoration in [removed.decor().prefix(), removed.decor().suffix()]
                .into_iter()
                .flatten()
            {
                let text = decoration.as_str().unwrap_or_default();
                if text.contains('#') {
                    trailing.push('\n');
                    trailing.push_str(text);
                    trailing.push('\n');
                }
            }
            doc.set_trailing(trailing);
        }
        let candidate = self.0.candidate(&doc)?;
        self.0.write_apply(&doc, candidate)
    }
    async fn reload_inner(&self) -> ApiResult<Value> {
        if let Some(host) = &self.0.host_config {
            let report = host.reload(None).await;
            let reply = {
                let _guard = self.0.files.lock().map_err(|_| io_error())?;
                self.0.payload()
            };
            let mut reply = reply.unwrap_or_else(|_| json!({}));
            reply["diff"] = serde_json::to_value(&report).map_err(|_| io_error())?;
            reply["applied"] = json!(report.applied);
            if !report.applied {
                reply["error"] = json!(report.summary());
            }
            return Ok(reply);
        }
        let _guard = self.0.files.lock().map_err(|_| io_error())?;
        let live = self.0.live.as_ref().ok_or_else(|| {
            ApiError(
                StatusCode::CONFLICT,
                "This host has no live provider configuration".into(),
            )
        })?;
        let _update = live.lock_update().map_err(|_| io_error())?;
        let config = live
            .startup_config()
            .reload()
            .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?;
        let candidate = live
            .prepare(config.clone())
            .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?;
        let document = self.0.document()?;
        let reply = self
            .0
            .payload_for(&config, &document, true, Some(candidate.main_model()))?;
        live.apply_prepared(candidate);
        Ok(reply)
    }
}
