//! The extensions file entry: one provider extension instance.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

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
