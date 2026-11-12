//! File layers retain their trust level when merging host configuration.
use super::*;
use crate::workspace_trust::{self, TrustSettings, TrustStatus, WorkspaceTrustState};
use std::sync::Mutex;

impl EffectiveConfig {
    pub(super) fn file(
        &mut self,
        path: &Path,
        layer: &str,
        required: bool,
        replacement: Option<(&Path, &str)>,
    ) -> Result<()> {
        let text = match replacement.filter(|(target, _)| *target == path) {
            Some((_, text)) => text.to_owned(),
            None => match std::fs::read_to_string(path) {
                Ok(text) => text,
                Err(error) if !required && error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(());
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("cannot read {layer} config {}", path.display()));
                }
            },
        };
        // Do not echo a parser diagnostic's source snippet; it can contain an accidentally supplied credential.
        let mut value: Value = toml::from_str(&text)
            .map_err(|_| anyhow::anyhow!("invalid TOML in {layer} config {}", path.display()))?;
        if layer == "project" {
            ignore_workspace_trust_keys(&mut value);
            ignore_hardline_key(&mut value);
            if !self.project_config_applies() {
                warn_skipped(&self.load_options.workspace, path, &value);
                return Ok(());
            }
            ignore_workspace_host_paths(&mut value);
        }
        let source = format!("{layer}: {}", path.display());
        self.apply(
            Override {
                path: String::new(),
                value,
                base: path.parent().map(Path::to_path_buf),
                replace: false,
            },
            &source,
        )
    }

    /// `workspace_trust` as the user, the environment and the CLI set it. The
    /// project layer is not applied yet and can never contribute. A value that
    /// does not parse leaves the gate on.
    fn trust_settings(&self) -> TrustSettings {
        let mut table = self
            .value
            .get("workspace_trust")
            .cloned()
            .unwrap_or_else(|| Value::Table(Default::default()));
        let options = &self.load_options;
        for key in ["enabled", "trusted", "trust_workspace"] {
            let path = format!("workspace_trust.{key}");
            let env_name = format!("BUBBLEPAW_WORKSPACE_TRUST__{}", key.to_ascii_uppercase());
            let value = options
                .cli
                .iter()
                .rev()
                .find(|entry| entry.path == path)
                .map(|entry| entry.value.clone())
                .or_else(|| {
                    options
                        .env
                        .iter()
                        .rev()
                        .find(|(name, _)| name.eq_ignore_ascii_case(&env_name))
                        .map(|(_, value)| parse_value(value))
                });
            if let (Some(value), Some(table)) = (value, table.as_table_mut()) {
                table.insert(key.to_owned(), value);
            }
        }
        table.try_into().unwrap_or_default()
    }

    /// Whether the workspace's project config applies to this load.
    fn project_config_applies(&self) -> bool {
        let settings = self.trust_settings();
        if !settings.enabled || settings.trust_workspace {
            return true;
        }
        let root = workspace_trust::canonical(&self.load_options.workspace);
        settings
            .trusted
            .iter()
            .any(|entry| workspace_trust::canonical(Path::new(entry)) == root)
            || self
                .load_options
                .home
                .as_deref()
                .is_some_and(|home| workspace_trust::is_trusted(home, &root))
    }

    /// What this load did with the workspace's project config, for hosts to show.
    pub fn workspace_trust(&self) -> WorkspaceTrustState {
        let mut state = self.trust_state();
        state.config_file = state.workspace.join(".bubblepaw/config.toml");
        state
    }

    fn trust_state(&self) -> WorkspaceTrustState {
        let workspace = workspace_trust::canonical(&self.load_options.workspace);
        let config_file = self.load_options.workspace.join(".bubblepaw/config.toml");
        let mut state = WorkspaceTrustState {
            status: TrustStatus::NoProjectConfig,
            workspace,
            config_file,
            sections: Vec::new(),
            how_to_trust: String::new(),
        };
        // `--config` replaces the user and project files: no project layer.
        if !self.config_paths().contains(&state.config_file) {
            return state;
        }
        let Ok(text) = std::fs::read_to_string(&state.config_file) else {
            return state;
        };
        if let Ok(table) = text.parse::<toml::Table>() {
            state.sections = table
                .keys()
                .filter(|key| *key != "workspace_trust")
                .cloned()
                .collect();
        }
        state.status = if !self.trust_settings().enabled {
            TrustStatus::GateOff
        } else if self.project_config_applies() {
            TrustStatus::Trusted
        } else {
            state.how_to_trust = how_to_trust(&state.workspace);
            TrustStatus::Untrusted
        };
        state
    }
}

fn how_to_trust(workspace: &Path) -> String {
    format!(
        "run `bubblepaw trust \"{}\"`, pass --trust-workspace for one run, or set workspace_trust.enabled=false in the user config",
        workspace.display()
    )
}

/// A project config file cannot decide its own trust.
fn ignore_workspace_trust_keys(value: &mut Value) {
    if value
        .as_table_mut()
        .is_some_and(|root| root.remove("workspace_trust").is_some())
    {
        tracing::warn!(
            "Ignoring workspace_trust in workspace .bubblepaw/config.toml; trust is decided by the user config, `bubblepaw trust` or --trust-workspace"
        );
    }
}

/// The hardline floor is user-only: a project config cannot lower it.
fn ignore_hardline_key(value: &mut Value) {
    let removed = value
        .get_mut("approvals")
        .and_then(Value::as_table_mut)
        .is_some_and(|approvals| approvals.remove("hardline").is_some());
    if removed {
        tracing::warn!(
            "Ignoring approvals.hardline in workspace .bubblepaw/config.toml; set it in the user config, the environment or the CLI"
        );
    }
}

/// One warning per workspace file and process, so a reload does not repeat it.
fn warn_skipped(workspace: &Path, file: &Path, value: &Value) {
    static SEEN: Mutex<std::collections::BTreeSet<PathBuf>> =
        Mutex::new(std::collections::BTreeSet::new());
    if !SEEN
        .lock()
        .is_ok_and(|mut seen| seen.insert(file.to_owned()))
    {
        return;
    }
    let sections = value
        .as_table()
        .map(|table| table.keys().cloned().collect::<Vec<_>>().join(", "))
        .unwrap_or_default();
    let root = workspace_trust::canonical(workspace);
    tracing::warn!(
        target: "bubblepaw::workspace_trust",
        workspace = %root.display(),
        file = %file.display(),
        sections = %sections,
        "Ignoring untrusted project config {} (sets: {sections}); to apply it, {}",
        file.display(),
        how_to_trust(&root)
    );
}

fn ignore_workspace_host_paths(value: &mut Value) {
    if remove_setting(value, "remote", "cloudflared") {
        tracing::warn!(
            "Ignoring remote.cloudflared in workspace .bubblepaw/config.toml; executable selection requires user configuration, BUBBLEPAW_CONFIG/--config, or the local remote.cloudflared.set method"
        );
    }
    if remove_setting(value, "storage", "data_dir") {
        tracing::warn!(
            "Ignoring storage.data_dir in workspace .bubblepaw/config.toml; host data directory selection requires user configuration, BUBBLEPAW_CONFIG/--config, or --data-dir"
        );
    }
}

fn remove_setting(value: &mut Value, section: &str, key: &str) -> bool {
    let Some(root) = value.as_table_mut() else {
        return false;
    };
    let Some(table) = root.get_mut(section).and_then(Value::as_table_mut) else {
        return false;
    };
    let removed = table.remove(key).is_some();
    if removed && table.is_empty() {
        // Empty tables replace whole sections in merge_at. Filtering the sole
        // forbidden key must leave the trusted section intact, not erase it.
        root.remove(section);
    }
    removed
}
