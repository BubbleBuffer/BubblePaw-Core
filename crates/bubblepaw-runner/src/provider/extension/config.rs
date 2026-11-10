//! The extensions file: one entry per provider extension instance.
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// One entry of the extensions file: `[{"id","command","args","cwd"?,"env"?}]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtensionConfig {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Variables set on this instance's process on top of the inherited
    /// environment, e.g. a per-account `CLAUDE_CONFIG_DIR`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// Names the variables an instance's `env` sets explicitly (comma-separated), so
/// an extension can tell them from inherited ones.
pub const EXTENSION_ENV_KEYS: &str = "BUBBLEPAW_EXTENSION_ENV_KEYS";

/// Whether `name` can be set on a child process and listed in [`EXTENSION_ENV_KEYS`].
pub fn valid_env_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['=', ',', '\0'])
}

/// Load an extensions file. A relative `cwd` is resolved against the file's directory.
pub fn load_extensions(path: impl AsRef<Path>) -> Result<Vec<ExtensionConfig>> {
    let path = path.as_ref();
    let bytes = std::fs::read(path)
        .with_context(|| format!("cannot read extensions file {}", path.display()))?;
    let mut entries: Vec<ExtensionConfig> = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid extensions file {}", path.display()))?;
    let base = path.parent().unwrap_or(Path::new("."));
    for entry in &mut entries {
        anyhow::ensure!(
            !entry.id.is_empty() && !entry.command.is_empty(),
            "extension entries need an id and a command"
        );
        anyhow::ensure!(
            entry.env.keys().all(|name| valid_env_name(name)),
            "extension env names must be nonempty and contain no '=', ',' or NUL"
        );
        if let Some(cwd) = &entry.cwd
            && cwd.is_relative()
        {
            entry.cwd = Some(base.join(cwd));
        }
    }
    Ok(entries)
}
