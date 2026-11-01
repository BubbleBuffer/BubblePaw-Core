//! `config.set`: change one key of the config file. The edit is validated through
//! the ordinary layers before the file is touched, written atomically with the
//! user's comments and ordering intact, then applied by the same reload path the
//! turn-start check uses.
use crate::{
    config::{
        EffectiveConfig, LeafError,
        redact::{ConfigTypeError, key_info},
    },
    live_config::{ReloadReport, approval_policy},
    provider_setup::ProviderSetup,
};
use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use toml_edit::{DocumentMut, Item, Table, TableLike};

/// Why a `config.set` was refused. The file is unchanged for every kind except
/// a `WriteFailed` that names a reload problem after the write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigSetError {
    /// The key is not in the schema, or names a section rather than a value.
    UnknownKey(String),
    /// The value has the wrong JSON/TOML type for the key.
    WrongType(String),
    /// The value has the right type but the resulting config is invalid.
    InvalidValue(String),
    /// The file could not be read or written, or the saved change failed to apply.
    WriteFailed(String),
    /// Any of the above, tied to the one leaf it is about (its full dotted key).
    AtKey(String, Box<ConfigSetError>),
}
impl ConfigSetError {
    /// Tie this refusal to `key`. A refusal already tied to a leaf keeps its own.
    #[must_use]
    pub fn at(self, key: impl Into<String>) -> Self {
        match self {
            Self::AtKey(..) => self,
            other => Self::AtKey(key.into(), Box::new(other)),
        }
    }
    /// The full dotted key of the leaf the refusal is about, when it is about
    /// exactly one; `None` otherwise. Sent as `data.key` in the host API.
    pub fn key(&self) -> Option<&str> {
        match self {
            Self::AtKey(key, _) => Some(key),
            _ => None,
        }
    }
    /// The host-API error code on every adapter; HTTP uses [`Self::status`].
    pub fn code(&self) -> i32 {
        match self {
            Self::UnknownKey(_) => -32010,
            Self::WrongType(_) => -32011,
            Self::InvalidValue(_) => -32012,
            Self::WriteFailed(_) => -32013,
            Self::AtKey(_, error) => error.code(),
        }
    }
    pub fn status(&self) -> u16 {
        match self {
            Self::WriteFailed(_) => 500,
            Self::AtKey(_, error) => error.status(),
            _ => 400,
        }
    }
    /// The stable name of the kind: `unknown_key`, `wrong_type`, `invalid_value`, `write_failed`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::UnknownKey(_) => "unknown_key",
            Self::WrongType(_) => "wrong_type",
            Self::InvalidValue(_) => "invalid_value",
            Self::WriteFailed(_) => "write_failed",
            Self::AtKey(_, error) => error.kind(),
        }
    }
}
impl std::fmt::Display for ConfigSetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownKey(m)
            | Self::WrongType(m)
            | Self::InvalidValue(m)
            | Self::WriteFailed(m) => f.write_str(m),
            Self::AtKey(_, error) => std::fmt::Display::fmt(error, f),
        }
    }
}
impl std::error::Error for ConfigSetError {}

/// Whether a running host applied the change or needs a restart for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Applied {
    Live,
    RestartRequired,
}

#[derive(Clone, Debug, Serialize)]
pub struct ConfigSetResult {
    pub key: String,
    pub applied: Applied,
    /// The reload that applied the edit; `None` when the file already held the value.
    pub reload: Option<ReloadReport>,
}

/// JSON to the TOML item the schema expects; `null` is handled by the caller.
fn to_item(value: &Value) -> Result<Item, ConfigSetError> {
    let wrong = |error: &dyn std::fmt::Display| ConfigSetError::WrongType(error.to_string());
    let value: toml::Value = serde_json::from_value(value.clone()).map_err(|e| wrong(&e))?;
    let mut doc: DocumentMut = format!("v = {value}")
        .parse()
        .map_err(|e: toml_edit::TomlError| wrong(&e))?;
    doc.remove("v")
        .ok_or_else(|| wrong(&"unrepresentable value"))
}

/// A map entry written as one table: every field of the object, nested
/// objects inline.
fn entry_item(fields: &serde_json::Map<String, Value>) -> Result<Item, ConfigSetError> {
    let mut table = Table::new();
    for (name, field) in fields {
        table.insert(name, to_item(field)?);
    }
    Ok(Item::Table(table))
}

/// Set (`Some`) or remove (`None`) the dotted `key` in a config document. Pure
/// over the document: comments, ordering and untouched keys stay. Removing the
/// last key of a table drops the emptied table, so a map entry can be deleted
/// leaf by leaf, or whole with `None` on the entry key.
pub fn edit_document(
    doc: &mut DocumentMut,
    key: &str,
    value: Option<&Value>,
) -> Result<(), ConfigSetError> {
    let info = key_info(key)
        .ok_or_else(|| ConfigSetError::UnknownKey(format!("Unknown config key: {key}")))?;
    // `null` removes a whole map entry and an object replaces it; any other
    // section stays refused.
    let whole_entry = info.entry && !info.settable;
    if !info.settable && !(whole_entry && value.is_none_or(Value::is_object)) {
        return Err(ConfigSetError::UnknownKey(format!(
            "{key} is a section; set one of its keys"
        )));
    }
    let item = match value {
        Some(Value::Object(fields)) if whole_entry => Some(entry_item(fields)?),
        value => value.map(to_item).transpose()?,
    };
    let parts: Vec<&str> = key.split('.').collect();
    edit_table(doc.as_table_mut(), &parts, item.as_ref(), key).map(|_| ())
}

/// Returns whether `table` is empty afterwards.
fn edit_table(
    table: &mut dyn TableLike,
    parts: &[&str],
    item: Option<&Item>,
    key: &str,
) -> Result<bool, ConfigSetError> {
    let (first, rest) = parts.split_first().expect("a key has a part");
    if rest.is_empty() {
        match item {
            Some(new) => {
                let mut new = new.clone();
                // Replace the value under its existing key: the key keeps its
                // leading comment, the value its trailing one.
                match table.get_mut(first) {
                    // A whole entry replaces the fields of the existing table in
                    // place, so its position and header comment stay.
                    Some(old) if old.is_table_like() && matches!(new, Item::Table(_)) => {
                        let Item::Table(fields) = new else {
                            unreachable!()
                        };
                        let old = old.as_table_like_mut().expect("checked table-like");
                        old.clear();
                        for (name, field) in fields {
                            old.insert(&name, field);
                        }
                    }
                    Some(old) => {
                        if let (Item::Value(old), Item::Value(next)) = (&*old, &mut new) {
                            *next.decor_mut() = old.decor().clone();
                        }
                        *old = new;
                    }
                    None => {
                        table.insert(first, new);
                    }
                }
            }
            None => {
                table.remove(first);
            }
        }
        return Ok(table.is_empty());
    }
    if !table.contains_key(first) {
        if item.is_none() {
            return Ok(table.is_empty());
        }
        let mut created = Table::new();
        created.set_implicit(true);
        table.insert(first, Item::Table(created));
    }
    let child = table
        .get_mut(first)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| {
            ConfigSetError::InvalidValue(format!("{key}: {first} holds a value, not a table"))
        })?;
    let emptied = edit_table(child, rest, item, key)?;
    if emptied && item.is_none() {
        table.remove(first);
    }
    Ok(table.is_empty())
}

fn classify(error: anyhow::Error) -> ConfigSetError {
    let message = format!("{error:#}");
    let refusal = if error.chain().any(|cause| cause.is::<ConfigTypeError>()) {
        ConfigSetError::WrongType(message)
    } else {
        ConfigSetError::InvalidValue(message)
    };
    match error
        .chain()
        .find_map(|cause| cause.downcast_ref::<LeafError>())
    {
        Some(leaf) => refusal.at(leaf.key.clone()),
        None => refusal,
    }
}

impl ProviderSetup {
    /// Validate, write and apply one key (`None` removes it so the default
    /// applies). Needs the host's live config: without one nothing is written.
    pub async fn config_set(&self, key: &str, value: Option<&Value>) -> Result<ConfigSetResult> {
        let live = key_info(key)
            .ok_or_else(|| ConfigSetError::UnknownKey(format!("Unknown config key: {key}")))?
            .live;
        let host = self
            .0
            .host_config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Config updates not available in this host"))?;
        let _host_update = host.lock_update().await;
        let written = {
            let _files = self
                .0
                .files
                .lock()
                .map_err(|_| ConfigSetError::WriteFailed("config file lock poisoned".into()))?;
            self.validate_and_write(key, value)?
        };
        let reload = if written {
            let report = host.check_locked(true, None).await;
            if !report.applied {
                return Err(ConfigSetError::WriteFailed(format!(
                    "saved to {}, but not applied: {}",
                    self.0.target.display(),
                    report.errors.join("; ")
                ))
                .into());
            }
            Some(report)
        } else {
            None
        };
        Ok(ConfigSetResult {
            key: key.into(),
            applied: if live {
                Applied::Live
            } else {
                Applied::RestartRequired
            },
            reload,
        })
    }

    /// Returns whether the file changed. Everything fallible precedes the write.
    fn validate_and_write(&self, key: &str, value: Option<&Value>) -> Result<bool, ConfigSetError> {
        let original = match std::fs::read_to_string(&self.0.target) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(_) => {
                return Err(ConfigSetError::WriteFailed(
                    "cannot read the config file".into(),
                ));
            }
        };
        let mut doc: DocumentMut = original
            .parse()
            .map_err(|_| ConfigSetError::WriteFailed("the config file is not valid TOML".into()))?;
        edit_document(&mut doc, key, value)?;
        if doc.to_string() == original {
            return Ok(false);
        }
        // A leaf write is about its own key unless validation names another.
        let leaf_write = key_info(key).is_some_and(|info| info.settable);
        self.validate(&doc).map_err(|error| match error {
            ConfigSetError::WrongType(_) | ConfigSetError::InvalidValue(_) if leaf_write => {
                error.at(key)
            }
            error => error,
        })?;
        self.0.write(&doc).map_err(|error| {
            ConfigSetError::WriteFailed(format!("cannot write the config file: {}", error.1))
        })?;
        Ok(true)
    }

    /// The edited file through every layer and every check a reload makes.
    fn validate(&self, doc: &DocumentMut) -> Result<(), ConfigSetError> {
        let candidate = EffectiveConfig::load_with_file(
            self.0.inputs.clone(),
            &self.0.target,
            &doc.to_string(),
        )
        .map_err(classify)?;
        let settings = candidate.settings().map_err(classify)?;
        approval_policy(&settings).map_err(classify)?;
        settings.mcp.validate().map_err(classify)?;
        if let Some(live) = &self.0.live {
            live.prepare(candidate).map_err(classify)?;
        }
        Ok(())
    }
}
