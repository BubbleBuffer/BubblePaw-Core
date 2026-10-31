//! The redacted effective tree behind `render` and `config.get`, and the key
//! classification (settable, live) `config.set` checks against the schema.
use super::{EffectiveConfig, RawSettings, merge_at};
use anyhow::Result;
use bubblepaw_core::config_schema::{Node, liveness, resolve};
use toml::Value;

/// A config value of the wrong TOML type for its key: the typed-error marker
/// `settings` raises when the merged tree does not deserialize.
#[derive(Debug)]
pub(crate) struct ConfigTypeError;
impl std::fmt::Display for ConfigTypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Config value has an invalid type")
    }
}
impl std::error::Error for ConfigTypeError {}

/// What the schema says about one key path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KeyInfo {
    /// A leaf (or free-form table) that holds a value; a section or map is not settable.
    pub settable: bool,
    /// A running host applies every key under this path without a restart.
    pub live: bool,
}

/// The schema entry for a dotted key, or `None` for an unknown key.
pub(crate) fn key_info(path: &str) -> Option<KeyInfo> {
    let node = resolve(RawSettings::FIELDS, &path.split('.').collect::<Vec<_>>())?;
    Some(KeyInfo {
        settable: matches!(node, Node::Leaf(_) | Node::Free(_)),
        live: liveness(node) == Some(true),
    })
}

/// One effective setting as `config.get` reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct SettingEntry {
    pub key: String,
    pub value: serde_json::Value,
    pub live: bool,
}

impl EffectiveConfig {
    /// The merged tree with credential-looking extension env values redacted and
    /// resolved paths filled in. No environment value is read here.
    pub(crate) fn redacted_value(&self) -> Result<Value> {
        let settings = self.settings()?;
        let mut effective = self.value.clone();
        // Extension env values are literal; keep credential-looking ones out of output.
        if let Some(Value::Table(extensions)) = effective.get_mut("extensions") {
            for extension in extensions.iter_mut().map(|(_, extension)| extension) {
                if let Some(Value::Table(env)) = extension.get_mut("env") {
                    for (name, value) in env.iter_mut() {
                        let upper = name.to_ascii_uppercase();
                        if ["KEY", "TOKEN", "SECRET", "PASSWORD"]
                            .iter()
                            .any(|marker| upper.contains(marker))
                        {
                            *value = Value::String("<redacted>".into());
                        }
                    }
                }
            }
        }
        merge_at(
            &mut effective,
            &["storage", "data_dir"],
            Value::String(settings.storage.data_dir.to_string_lossy().into_owned()),
        )?;
        for extension in settings.extensions {
            if let Some(cwd) = extension.cwd {
                merge_at(
                    &mut effective,
                    &["extensions", &extension.id, "cwd"],
                    Value::String(cwd.to_string_lossy().into_owned()),
                )?;
            }
        }
        for (name, provider) in settings.providers {
            if let Some(path) = provider.auth_file {
                merge_at(
                    &mut effective,
                    &["providers", &name, "auth_file"],
                    Value::String(path.to_string_lossy().into_owned()),
                )?;
            }
        }
        Ok(effective)
    }

    /// Every leaf of the redacted tree with its liveness, sorted by key.
    pub fn setting_entries(&self) -> Result<Vec<SettingEntry>> {
        let mut out = Vec::new();
        leaves(&self.redacted_value()?, &mut Vec::new(), &mut out);
        Ok(out)
    }
}

fn leaves(value: &Value, parts: &mut Vec<String>, out: &mut Vec<SettingEntry>) {
    match value {
        Value::Table(table) if !table.is_empty() => {
            for (key, value) in table {
                parts.push(key.clone());
                leaves(value, parts, out);
                parts.pop();
            }
        }
        _ => {
            let key = parts.join(".");
            out.push(SettingEntry {
                live: key_info(&key).is_some_and(|info| info.live),
                value: serde_json::to_value(value).unwrap_or(serde_json::Value::Null),
                key,
            });
        }
    }
}
