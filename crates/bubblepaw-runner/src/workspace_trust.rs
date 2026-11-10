//! Workspace trust: whether a workspace's own `.bubblepaw/config.toml` may change this host.
//!
//! Opening someone else's repository must not let its config pick providers,
//! endpoints, approval rules or MCP servers. The project config layer applies
//! only for a trusted workspace: one recorded in the user-level store
//! (`<home>/.bubblepaw/trusted-workspaces.toml`, written by `bubblepaw trust`),
//! listed in the user config key `workspace_trust.trusted`, or trusted for one
//! run (`--trust-workspace`, `workspace_trust.trust_workspace`). The gate is
//! decided in the config loader (`config/file_layers.rs`); this module owns the
//! store and the settings keys. Project config cannot set these keys, and
//! nothing a model can call writes the store.
use anyhow::{Context, Result};
use bubblepaw_core::{
    config_schema::{Node, RESTART},
    config_section,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// File name of the trust store inside `<home>/.bubblepaw/`.
pub const STORE_FILE: &str = "trusted-workspaces.toml";

config_section! {
    /// Which workspaces' project config applies. A project config file cannot set these keys.
    #[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
    #[serde(default)]
    pub struct TrustSettings {
        /// `false` applies every workspace's project config, as before the gate existed.
        pub enabled: bool => RESTART,
        /// Workspace roots whose project config applies, besides those `bubblepaw trust` recorded.
        pub trusted: Vec<String> => RESTART,
        /// Trust this run's workspace without recording it (`--trust-workspace`).
        pub trust_workspace: bool => RESTART,
    }
}
impl Default for TrustSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            trusted: Vec::new(),
            trust_workspace: false,
        }
    }
}

/// One trusted workspace root and when it was trusted (RFC 3339).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrustRecord {
    pub path: String,
    pub trusted_at: String,
}

#[derive(Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    workspace: Vec<TrustRecord>,
}

/// What the loader did with a workspace's project config.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrustStatus {
    /// The workspace has no `.bubblepaw/config.toml` (or `--config` replaced the layers): nothing to decide.
    NoProjectConfig,
    /// `workspace_trust.enabled = false`: project config applies without a decision.
    GateOff,
    /// The project config applies.
    Trusted,
    /// The project config is skipped until the workspace is trusted.
    Untrusted,
}

/// The trust state of one workspace, as shown by `config.get` and `bubblepaw config`-style clients.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct WorkspaceTrustState {
    pub status: TrustStatus,
    pub workspace: PathBuf,
    pub config_file: PathBuf,
    /// Top-level sections the project config file sets.
    pub sections: Vec<String>,
    /// How to trust the workspace; empty unless `status` is `untrusted`.
    pub how_to_trust: String,
}

/// The user's home directory, the same way the CLI and `HostConfig` find it.
pub fn default_home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// Where the store lives under `home`.
pub fn store_path(home: &Path) -> PathBuf {
    home.join(".bubblepaw").join(STORE_FILE)
}

/// The workspace root as the store records it: symlinks resolved when the path exists.
pub fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_owned())
}

fn read(home: &Path) -> Result<StoreFile> {
    let path = store_path(home);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StoreFile::default());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("cannot read {}", path.display()));
        }
    };
    toml::from_str(&text).with_context(|| format!("invalid TOML in {}", path.display()))
}

fn write(home: &Path, file: &StoreFile) -> Result<()> {
    let path = store_path(home);
    std::fs::create_dir_all(path.parent().context("store path has no parent")?)?;
    let temporary = path.with_extension("toml.tmp");
    std::fs::write(&temporary, toml::to_string(file)?)
        .with_context(|| format!("cannot write {}", temporary.display()))?;
    std::fs::rename(&temporary, &path).with_context(|| format!("cannot replace {}", path.display()))
}

fn same(entry: &str, root: &Path) -> bool {
    canonical(Path::new(entry)) == root
}

/// The workspaces `bubblepaw trust` recorded.
pub fn list(home: &Path) -> Result<Vec<TrustRecord>> {
    Ok(read(home)?.workspace)
}

/// Whether `root` (a canonical path) is in the store. An unreadable store trusts nothing.
pub fn is_trusted(home: &Path, root: &Path) -> bool {
    read(home).is_ok_and(|file| file.workspace.iter().any(|record| same(&record.path, root)))
}

/// Record `workspace` as trusted. Trusting a trusted workspace keeps its first time.
pub fn trust(home: &Path, workspace: &Path) -> Result<TrustRecord> {
    let root = canonical(workspace);
    let mut file = read(home)?;
    if let Some(record) = file
        .workspace
        .iter()
        .find(|record| same(&record.path, &root))
    {
        return Ok(record.clone());
    }
    let record = TrustRecord {
        path: root.to_string_lossy().into_owned(),
        trusted_at: chrono::Utc::now().to_rfc3339(),
    };
    file.workspace.push(record.clone());
    write(home, &file)?;
    Ok(record)
}

/// Remove `workspace` from the store; whether it was there.
pub fn untrust(home: &Path, workspace: &Path) -> Result<bool> {
    let root = canonical(workspace);
    let mut file = read(home)?;
    let before = file.workspace.len();
    file.workspace.retain(|record| !same(&record.path, &root));
    let removed = file.workspace.len() != before;
    if removed {
        write(home, &file)?;
    }
    Ok(removed)
}
