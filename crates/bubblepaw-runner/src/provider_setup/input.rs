//! Saved-provider input: the JSON the setup endpoints accept, its checks, and
//! the TOML table it is written as.
use super::{ApiError, ApiResult, bad, valid_name};
use crate::config::{ProviderModelSettings, ProviderSettings};
use anyhow::{Result, ensure};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path as FilePath, PathBuf},
};
use toml_edit::{DocumentMut, Item, Table, value};

fn valid_env(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && name
            .bytes()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}
pub(crate) fn loopback_url(base: &reqwest::Url) -> bool {
    base.host_str().is_some_and(|host| {
        host == "localhost"
            || host.ends_with(".localhost")
            || host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}
/// Apply before reading any credential, including for providers loaded from disk.
pub(super) fn validate_setup(
    provider: &ProviderSettings,
    allowed_key_envs: &[String],
) -> ApiResult<()> {
    if let Some(base) = &provider.base_url {
        let base = reqwest::Url::parse(base).map_err(|_| bad())?;
        if !matches!(base.scheme(), "http" | "https")
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(bad());
        }
        if base.scheme() != "https" && !loopback_url(&base) {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "base_url must use https unless it points at this machine (localhost/127.0.0.1/::1)"
                    .into(),
            ));
        }
    }
    if let Some(name) = &provider.key_env {
        if !valid_env(name) {
            return Err(bad());
        }
        if !["_API_KEY", "_KEY", "_TOKEN"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
            && !allowed_key_envs.contains(name)
        {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!(
                    "key_env {name} is not an allowed key variable; names must end in _API_KEY, _KEY or _TOKEN, or be listed in [web] provider_key_envs"
                ),
            ));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProviderInput {
    kind: String,
    base_url: Option<String>,
    key_env: Option<String>,
    auth_file: Option<PathBuf>,
    #[serde(default)]
    reasoning: BTreeMap<String, Value>,
    models: Vec<ProviderModelSettings>,
}
impl ProviderInput {
    pub(super) fn settings(mut self, name: &str, target: &FilePath) -> Result<ProviderSettings> {
        ensure!(valid_name(name), "Invalid provider name");
        self.base_url = self.base_url.filter(|value| !value.is_empty());
        self.key_env = self.key_env.filter(|value| !value.is_empty());
        self.auth_file = self.auth_file.filter(|value| !value.as_os_str().is_empty());
        ensure!(
            self.key_env.as_deref().is_none_or(valid_env),
            "Invalid environment variable name"
        );
        let mut reasoning = BTreeMap::new();
        for (key, val) in self.reasoning {
            ensure!(
                matches!(key.as_str(), "thinking" | "reasoning_effort"),
                "Unknown reasoning field"
            );
            if let Some(table) = val.as_object() {
                ensure!(
                    key == "thinking" && table.keys().all(|key| key == "type"),
                    "Unknown reasoning field"
                );
            }
            reasoning.insert(key, toml::Value::try_from(val)?);
        }
        let auth_file = self.auth_file.map(|path| {
            if path.is_relative() {
                target.parent().unwrap_or(FilePath::new(".")).join(path)
            } else {
                path
            }
        });
        let settings = ProviderSettings {
            account: None,
            kind: Some(self.kind),
            base_url: self.base_url,
            key_env: self.key_env,
            auth_file,
            reasoning,
            models: self.models,
        };
        Ok(settings)
    }
}

fn model_json(model: &ProviderModelSettings) -> Value {
    let mut value = json!({"id": model.id});
    if let Some(enabled) = model.image_input {
        value["image_input"] = json!(enabled);
    }
    if let Some(policy) = model.reasoning_replay {
        value["reasoning_replay"] = json!(policy);
    }
    value
}

pub(super) fn provider_table(
    provider: &ProviderSettings,
    previous: Option<&Item>,
) -> ApiResult<Item> {
    // Omitted optional scalars are removed from the target table. The save
    // handler refuses such removal if a lower layer would supply them again.
    let mut table = Table::new();
    table.insert(
        "kind",
        value(provider.kind.as_deref().unwrap_or("chat-completions")),
    );
    if let Some(base) = &provider.base_url {
        table.insert("base_url", value(base));
    }
    if let Some(key) = &provider.key_env {
        table.insert("key_env", value(key));
    }
    if let Some(path) = &provider.auth_file {
        table.insert("auth_file", value(path.to_string_lossy().into_owned()));
    }
    let fields = json!({
        "reasoning": provider.reasoning,
        "models": provider.models.iter().map(model_json).collect::<Vec<_>>(),
    });
    let fields = toml::Value::try_from(fields).map_err(|_| bad())?;
    let encoded = toml::to_string(&fields).map_err(|_| bad())?;
    let mut fields: DocumentMut = encoded.parse().map_err(|_| bad())?;
    table.insert(
        "reasoning",
        fields
            .remove("reasoning")
            .unwrap_or_else(|| value(toml_edit::InlineTable::new())),
    );
    table.insert(
        "models",
        fields
            .remove("models")
            .unwrap_or_else(|| value(toml_edit::Array::new())),
    );
    if let Some(previous) = previous.and_then(Item::as_table) {
        *table.decor_mut() = previous.decor().clone();
        if let Some(position) = previous.position() {
            table.set_position(Some(position));
        }
        for (key, item) in table.iter_mut() {
            if let Some(old) = previous.get(key.get()) {
                match (old, item) {
                    (Item::Value(old), Item::Value(new)) => *new.decor_mut() = old.decor().clone(),
                    (Item::Table(old), Item::Table(new)) => *new.decor_mut() = old.decor().clone(),
                    _ => {}
                }
            }
        }
    }
    Ok(Item::Table(table))
}
