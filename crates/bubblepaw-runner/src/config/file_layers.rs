//! File layers retain their trust level when merging host configuration.
use super::*;

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
