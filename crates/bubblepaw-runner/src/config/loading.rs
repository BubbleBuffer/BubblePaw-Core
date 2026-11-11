//! Loading, layering and applying overrides to the effective config.
use super::*;

impl EffectiveConfig {
    pub fn load(options: LoadOptions) -> Result<Self> {
        let result = Self::layer(options)?;
        result.settings()?;
        Ok(result)
    }
    /// Merge known keys without converting typed values yet. This lets the CLI
    /// locate a selected provider before mapping its base-url/auth-file aliases.
    pub fn layer(options: LoadOptions) -> Result<Self> {
        Self::layer_with_file(options, None)
    }
    /// Validate an edited file through the ordinary layers before replacing it.
    pub fn load_with_file(options: LoadOptions, path: &Path, text: &str) -> Result<Self> {
        let result = Self::layer_with_file(options, Some((path, text)))?;
        result.settings()?;
        Ok(result)
    }
    fn layer_with_file(options: LoadOptions, replacement: Option<(&Path, &str)>) -> Result<Self> {
        let mut result = Self {
            load_options: options.clone(),
            environment: RuntimeEnvironment(options.env.clone()),
            attach_environment: None,
            default_codex_auth_file: None,
            origin_directory: None,
            value: toml::from_str(DEFAULTS)?,
            sources: BTreeMap::new(),
            bases: BTreeMap::new(),
        };
        collect_sources(&result.value, "", "default", &mut result.sources);
        let custom = options.custom_file.or_else(|| {
            options
                .env
                .iter()
                .find(|(name, _)| name == "BUBBLEPAW_CONFIG")
                .map(|(_, path)| PathBuf::from(path))
        });
        if let Some(path) = custom {
            result.file(&path, "custom", true, replacement)?;
        } else {
            if let Some(home) = options.home {
                result.file(
                    &home.join(".bubblepaw/config.toml"),
                    "user",
                    false,
                    replacement,
                )?;
            }
            result.file(
                &options.workspace.join(".bubblepaw/config.toml"),
                "project",
                false,
                replacement,
            )?;
        }
        // Key names can themselves be configured by environment/CLI. Determine
        // the names before interpreting BUBBLEPAW_* as config paths.
        let mut credential_names = key_names(&result.value);
        for (name, value) in &options.env {
            if let Some(path) = name.strip_prefix("BUBBLEPAW_") {
                collect_key_names(
                    &parse_value(value),
                    &path.to_ascii_lowercase().replace("__", "."),
                    &mut credential_names,
                );
            }
        }
        for entry in &options.cli {
            collect_key_names(&entry.value, &entry.path, &mut credential_names);
        }
        result
            .load_options
            .env
            .retain(|(name, _)| name.starts_with("BUBBLEPAW_") && !credential_names.contains(name));
        let mut env = options.env;
        env.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, value) in env {
            if name == "BUBBLEPAW_CONFIG"
                || name == "BUBBLEPAW_PROVIDER_DUMP_DIR"
                || credential_names.contains(&name)
            {
                continue;
            }
            if let Some(path) = name.strip_prefix("BUBBLEPAW_") {
                result.apply(
                    Override::value(
                        path.to_ascii_lowercase().replace("__", "."),
                        parse_value(&value),
                    ),
                    "env",
                )?;
            }
        }
        for entry in options.cli {
            result.apply(entry, "CLI")?;
        }
        Ok(result)
    }
    pub(super) fn apply(&mut self, mut entry: Override, source: &str) -> Result<()> {
        validate_keys(&entry.value, &entry.path)?;
        if entry.path.is_empty() && entry.value.as_table().is_some_and(|table| table.is_empty()) {
            return Ok(());
        }
        if !entry.path.is_empty() {
            ensure!(
                known_path(&entry.path),
                "Unknown config key: {}",
                entry.path
            );
        }
        normalize_roles(&mut entry.value, &entry.path);
        let parts: Vec<_> = entry
            .path
            .split('.')
            .filter(|part| !part.is_empty())
            .collect();
        if entry.replace {
            merge_at(&mut self.value, &parts, Value::Table(toml::map::Map::new()))?;
            let prefix = format!("{}.", entry.path);
            self.sources
                .retain(|path, _| path != &entry.path && !path.starts_with(&prefix));
            self.bases
                .retain(|path, _| path != &entry.path && !path.starts_with(&prefix));
        }
        merge_at(&mut self.value, &parts, entry.value.clone())?;
        // Tables merge recursively; scalar/array replacements discard obsolete provenance.
        record_override(
            &entry.value,
            &entry.path,
            source,
            entry.base.as_deref(),
            &mut self.sources,
            &mut self.bases,
        );
        Ok(())
    }
}
