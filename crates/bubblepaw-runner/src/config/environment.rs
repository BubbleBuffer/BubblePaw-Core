//! Environment lookup, attach preparation and value access on the effective config.
use super::*;

impl EffectiveConfig {
    /// Credential lookup uses the invoking client's environment snapshot. Never
    /// mutate the host environment while another session is running.
    pub fn environment(&self, name: &str) -> Option<String> {
        self.attach_environment
            .as_ref()
            .unwrap_or(&self.environment)
            .0
            .iter()
            .rev()
            .find(|(key, _)| {
                if cfg!(windows) {
                    key.eq_ignore_ascii_case(name)
                } else {
                    key == name
                }
            })
            .map(|(_, value)| value.clone())
    }
    /// Private process inputs for execution; excluded from config diagnostics.
    pub fn runtime_environment(&self) -> Vec<(String, String)> {
        let Some(forwarded) = &self.attach_environment else {
            return self.environment.0.clone();
        };
        // MCP credentials belong only to this request's MCP instances. Other
        // subprocesses inherit the host environment with only the non-MCP
        // references overlaid. A name referenced both ways is overlaid too, as
        // a standalone subprocess would see it.
        let names = self.non_mcp_environment_names();
        let mut environment: BTreeMap<_, _> = std::env::vars().collect();
        for (name, value) in &forwarded.0 {
            let referenced = names.iter().any(|other| {
                if cfg!(windows) {
                    other.eq_ignore_ascii_case(name)
                } else {
                    other == name
                }
            });
            if referenced {
                if cfg!(windows) {
                    environment.retain(|existing, _| !existing.eq_ignore_ascii_case(name));
                }
                environment.insert(name.clone(), value.clone());
            }
        }
        environment.into_iter().collect()
    }

    /// Credential names referenced outside `[mcp]`: provider and web keys,
    /// plus the Crawl4AI token when that backend is configured.
    fn non_mcp_environment_names(&self) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        if let Value::Table(table) = &self.value {
            for (key, value) in table.iter().filter(|(key, _)| key.as_str() != "mcp") {
                collect_key_names(value, key, &mut names);
            }
        }
        if self
            .value("web.fetch_backend")
            .is_some_and(|backend| match backend {
                Value::String(backend) => backend.starts_with("crawl4ai:"),
                Value::Array(backends) => backends
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|backend| backend.starts_with("crawl4ai:")),
                _ => false,
            })
        {
            names.insert("CRAWL4AI_API_TOKEN".into());
        }
        names
    }

    fn mcp_environment_names(&self) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        if let Some(mcp) = self.value.get("mcp") {
            collect_key_names(mcp, "mcp", &mut names);
        }
        names
    }

    /// Only configured MCP references cross into the request's MCP host.
    pub fn mcp_environment(&self) -> Vec<(String, String)> {
        self.mcp_environment_names()
            .into_iter()
            .filter_map(|name| self.environment(&name).map(|value| (name, value)))
            .collect()
    }

    pub fn codex_models_cache(&self) -> Option<PathBuf> {
        self.default_codex_auth_file
            .as_ref()
            .and_then(|path| path.parent().map(Path::to_owned))
            .or_else(|| self.environment("CODEX_HOME").map(PathBuf::from))
            .or_else(|| {
                self.environment("USERPROFILE")
                    .or_else(|| self.environment("HOME"))
                    .map(|path| PathBuf::from(path).join(".codex"))
            })
            .map(|path| path.join("models_cache.json"))
    }

    /// Freeze the allowlisted environment inputs before local RPC submission.
    /// Missing names remain absent, so credential lookup cannot fall back to
    /// the owning process's values. Noncredential config layers already live
    /// in load_options and the resolved value/provenance tables.
    pub fn prepare_for_attach(&mut self) {
        if self.attach_environment.is_none() {
            self.default_codex_auth_file = self
                .environment("CODEX_HOME")
                .map(|path| PathBuf::from(path).join("auth.json"))
                .or_else(|| {
                    self.environment("USERPROFILE")
                        .or_else(|| self.environment("HOME"))
                        .map(|path| PathBuf::from(path).join(".codex/auth.json"))
                });
            if let Some(path) = &mut self.default_codex_auth_file
                && path.is_relative()
                && let Some(origin) = &self.origin_directory
            {
                *path = origin.join(&*path);
            }
        }
        let mut names = self.non_mcp_environment_names();
        names.extend(self.mcp_environment_names());
        self.attach_environment = Some(RuntimeEnvironment(
            names
                .into_iter()
                .filter_map(|name| self.environment(&name).map(|value| (name, value)))
                .collect(),
        ));
        self.environment.0.clear();
    }
    /// Freeze relative layer paths against the invoking process, before sending
    /// configuration to a host whose current directory may be different.
    pub fn anchor_paths(&mut self, directory: &Path) {
        self.origin_directory = Some(directory.to_owned());
        let absolute = |path: &mut PathBuf| {
            if path.is_relative() {
                *path = directory.join(&*path);
            }
        };
        absolute(&mut self.load_options.workspace);
        if let Some(path) = &mut self.load_options.home {
            absolute(path);
        }
        if let Some(path) = &mut self.load_options.custom_file {
            absolute(path);
        }
        for (name, value) in self
            .load_options
            .env
            .iter_mut()
            .chain(self.environment.0.iter_mut())
        {
            if matches!(
                name.as_str(),
                "BUBBLEPAW_CONFIG" | "CODEX_HOME" | "HOME" | "USERPROFILE"
            ) {
                let mut path = PathBuf::from(&*value);
                absolute(&mut path);
                *value = path.to_string_lossy().into_owned();
            }
        }
        for entry in &mut self.load_options.cli {
            absolute(entry.base.get_or_insert_with(|| directory.to_owned()));
        }
        for path in self.sources.keys() {
            absolute(
                self.bases
                    .entry(path.clone())
                    .or_insert_with(|| directory.to_owned()),
            );
        }
    }

    pub fn reload(&self) -> Result<Self> {
        let mut options = self.load_options();
        if let Some(forwarded) = &self.attach_environment {
            options.env.extend(forwarded.0.clone());
        } else {
            options.env = self.environment.0.clone();
        }
        let mut next = Self::load(options)?;
        next.attach_environment = self.attach_environment.clone();
        next.default_codex_auth_file = self.default_codex_auth_file.clone();
        if let Some(directory) = &self.origin_directory {
            next.anchor_paths(directory);
        }
        Ok(next)
    }

    /// Valid TOML with dotted assignments and provenance. No environment value is read here.
    pub fn render(&self) -> Result<String> {
        let effective = self.redacted_value()?;
        let mut output = String::new();
        render_leaves(&effective, &mut Vec::new(), &self.sources, &mut output);
        Ok(output)
    }
    pub fn source(&self, path: &str) -> Option<&str> {
        self.sources.get(path).map(String::as_str)
    }
    /// Original layering inputs, excluding credentials and unrelated environment values.
    pub fn load_options(&self) -> LoadOptions {
        self.load_options.clone()
    }
    /// All disk layers, including optional files that do not exist yet.
    pub fn config_paths(&self) -> Vec<PathBuf> {
        let options = &self.load_options;
        if let Some(path) = options.custom_file.clone().or_else(|| {
            options
                .env
                .iter()
                .find(|(name, _)| name == "BUBBLEPAW_CONFIG")
                .map(|(_, path)| PathBuf::from(path))
        }) {
            return vec![path];
        }
        let mut paths = Vec::new();
        if let Some(home) = &options.home {
            paths.push(home.join(".bubblepaw/config.toml"));
        }
        paths.push(options.workspace.join(".bubblepaw/config.toml"));
        paths
    }
    pub fn value(&self, path: &str) -> Option<&Value> {
        path.split('.')
            .try_fold(&self.value, |value, key| value.get(key))
    }
}
