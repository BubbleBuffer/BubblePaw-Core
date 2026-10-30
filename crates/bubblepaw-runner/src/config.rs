//! Runner-owned layered settings. Credentials are named, never read by the loader.
use crate::{
    model_roles::ModelRoles,
    net::{FetchBackend, SearchBackend, WebConfig, WebExpose, one_or_many},
    provider::{
        ChatCompletionsProvider, ExtensionConfig, MessagesProvider, OpenAiDecisions,
        OpenCodeDecisions, OpenRouterDecisions, ProviderEndpoints, ProviderRegistry,
        ResponsesProvider, load_extensions,
    },
};
use anyhow::{Context, Result, bail, ensure};
use bubblepaw_core::{ContextStrategy, ExecutionMode, ReasoningEffort, ReasoningReplay};
use bubblepaw_mcp::McpSettings;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};
use toml::Value;

#[path = "config_keys.rs"]
mod keys;
#[path = "config/live_settings.rs"]
mod live_settings;
#[path = "config/roles.rs"]
mod roles;
use keys::known_path;
use roles::resolve_role;

const DEFAULTS: &str = include_str!("config/defaults.toml");

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Override {
    pub path: String,
    pub value: Value,
    base: Option<PathBuf>,
    replace: bool,
}
impl Override {
    pub fn value(path: impl Into<String>, value: Value) -> Self {
        Self {
            path: path.into(),
            value,
            base: None,
            replace: false,
        }
    }
    /// Compatibility adapters with whole-object semantics can replace a table
    /// while ordinary TOML configuration continues to merge individual keys.
    pub fn replace(path: impl Into<String>, value: Value) -> Self {
        Self {
            replace: true,
            ..Self::value(path, value)
        }
    }
    pub fn set(argument: &str) -> Result<Self> {
        let (path, value) = argument
            .split_once('=')
            .context("--set requires key.path=value")?;
        ensure!(!path.is_empty(), "--set requires a nonempty key path");
        Ok(Self::value(path, parse_value(value)))
    }
    pub fn extensions_json(path: impl AsRef<Path>) -> Result<Self> {
        let mut entries = toml::map::Map::new();
        for extension in load_extensions(path)? {
            ensure!(
                !entries.contains_key(&extension.id),
                "Duplicate extension id"
            );
            let mut entry = toml::map::Map::new();
            entry.insert("command".into(), Value::String(extension.command));
            if let Some(account) = extension.account {
                entry.insert("account".into(), Value::String(account));
            }
            entry.insert(
                "args".into(),
                Value::Array(extension.args.into_iter().map(Value::String).collect()),
            );
            if let Some(cwd) = extension.cwd {
                entry.insert(
                    "cwd".into(),
                    Value::String(cwd.to_string_lossy().into_owned()),
                );
            }
            if !extension.env.is_empty() {
                entry.insert(
                    "env".into(),
                    Value::Table(
                        extension
                            .env
                            .into_iter()
                            .map(|(name, value)| (name, Value::String(value)))
                            .collect(),
                    ),
                );
            }
            entries.insert(extension.id, Value::Table(entry));
        }
        Ok(Self::value("extensions", Value::Table(entries)))
    }
}

/// The environment and CLI are supplied explicitly so tests need no process-global mutation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoadOptions {
    pub workspace: PathBuf,
    pub home: Option<PathBuf>,
    pub custom_file: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub cli: Vec<Override>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
struct RuntimeEnvironment(Vec<(String, String)>);
impl std::fmt::Debug for RuntimeEnvironment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<private runtime environment>")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EffectiveConfig {
    load_options: LoadOptions,
    // A local invocation may inspect its full environment, but that snapshot
    // must never be serialized into an attach request.
    #[serde(skip)]
    environment: RuntimeEnvironment,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attach_environment: Option<RuntimeEnvironment>,
    /// Resolve the implicit Codex auth path on the client, just like other
    /// config paths, instead of forwarding HOME or CODEX_HOME wholesale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_codex_auth_file: Option<PathBuf>,
    #[serde(default)]
    origin_directory: Option<PathBuf>,
    value: Value,
    sources: BTreeMap<String, String>,
    bases: BTreeMap<String, PathBuf>,
}
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
            value: DEFAULTS.parse()?,
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
    fn file(
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
        let value: Value = text
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid TOML in {layer} config {}", path.display()))?;
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
    fn apply(&mut self, entry: Override, source: &str) -> Result<()> {
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
    pub fn settings(&self) -> Result<Settings> {
        self.settings_for_main(None)
    }
    pub fn settings_for_main(&self, main: Option<&str>) -> Result<Settings> {
        validate_mcp(self.value.get("mcp"))?;
        let mut raw: RawSettings = self
            .value
            .clone()
            .try_into()
            .map_err(|_| anyhow::anyhow!("Config value has an invalid type"))?;
        ensure!(
            !raw.model.main.trim().is_empty(),
            "model.main must not be empty"
        );
        ensure!(
            !raw.model.provider.trim().is_empty(),
            "model.provider must not be empty"
        );
        ensure!(
            raw.agent.max_rounds != Some(0),
            "agent.max_rounds must be positive"
        );
        let qualified = main.map(str::to_owned).unwrap_or_else(|| {
            if raw.model.main.contains('/') {
                raw.model.main.clone()
            } else {
                format!("{}/{}", raw.model.provider, raw.model.main)
            }
        });
        let mut fallback_ids = BTreeSet::new();
        for fallback in &raw.model.fallbacks {
            ensure!(
                fallback
                    .split_once('/')
                    .is_some_and(|(provider, model)| !provider.trim().is_empty()
                        && !model.trim().is_empty()
                        && (raw
                            .providers
                            .get(provider)
                            .is_some_and(|settings| !matches!(
                                settings.kind.as_deref(),
                                Some(
                                    "decisions"
                                        | "openai-decisions"
                                        | "openrouter-decisions"
                                        | "opencode-decisions"
                                )
                            ))
                            || raw.extensions.contains_key(provider))),
                "model.fallbacks must name configured LLM routes as <provider>/<model>"
            );
            ensure!(
                fallback != &qualified && fallback_ids.insert(fallback),
                "model.fallbacks must not repeat the primary or another fallback"
            );
        }
        // Keep the legacy disabled-endpoint switch at the reference boundary,
        // so custom chapter/decision references follow the selected summary too.
        if raw
            .providers
            .get("opencode")
            .and_then(|p| p.base_url.as_deref())
            .is_some_and(|url| url.trim().is_empty())
            && self
                .sources
                .get("roles.summary.models")
                .is_some_and(|source| source == "default")
            && let Some(RoleEntry::Table(summary)) = raw.roles.get_mut("summary")
        {
            summary.models = vec!["main".into()];
        }
        let mut roles =
            ModelRoles::for_models("opencode/deepseek-v4.1-flash".into(), qualified.clone());
        for (name, target) in [
            ("summary", &mut roles.summary),
            ("chapter", &mut roles.chapter),
            ("decider", &mut roles.decider),
            ("approver", &mut roles.approver),
        ] {
            let entry = raw
                .roles
                .get(name)
                .with_context(|| format!("Missing config key: roles.{name}"))?;
            target.models = resolve_role(name, &raw.roles, &qualified, &mut Vec::new())?;
            if let RoleEntry::Table(entry) = entry {
                target.reasoning_effort = entry.reasoning_effort.unwrap_or_default();
                target.max_output_tokens = entry.max_output_tokens;
                if let Some(repeats) = entry.repeats {
                    target.repeats = Some(repeats);
                }
                if let Some(threshold) = entry.threshold {
                    target.threshold = Some(threshold);
                }
            }
        }
        roles.validate().map_err(crate::host_config::role_error)?;
        let strategy = match raw.context.strategy.as_str() {
            "lcm" => ContextStrategy::Lcm,
            "classical" => ContextStrategy::Classical,
            value => {
                bail!("Invalid config value: context.strategy={value:?}; expected lcm or classical")
            }
        };
        ensure!(
            raw.context.compact_threshold_tokens > 0,
            "context.compact_threshold_tokens must be positive"
        );
        ensure!(
            raw.context.window_tokens.is_none_or(|n| n > 0),
            "context.window_tokens must be positive"
        );
        ensure!(
            raw.context.classical.verbatim_user_tokens > 0,
            "context.classical.verbatim_user_tokens must be positive"
        );
        raw.context.lcm.validate()?;
        for (name, provider) in &mut raw.providers {
            provider.validate(name)?;
            if let Some(path) = provider
                .auth_file
                .as_ref()
                .filter(|path| path.is_relative())
                && let Some(base) = self.bases.get(&format!("providers.{name}.auth_file"))
            {
                provider.auth_file = Some(base.join(path));
            }
            if provider.kind.as_deref() == Some("codex") && provider.auth_file.is_none() {
                provider.auth_file = self.default_codex_auth_file.clone();
            }
        }
        for mode in std::iter::once(&raw.approvals.confirm).chain(raw.approvals.tools.values()) {
            ensure!(
                matches!(mode.as_str(), "ask" | "allow" | "deny" | "auto"),
                "Invalid approval mode"
            );
        }
        ensure!(
            raw.approvals
                .deny
                .iter()
                .all(|rule| !rule.trim().is_empty()),
            "approvals.deny entries must be nonempty exact tool names"
        );
        raw.agents.validate()?;
        raw.web.validate()?;
        raw.delegate.validate()?;
        raw.exec.validate()?;
        for directory in &mut raw.skills.dirs {
            if directory.is_relative() {
                *directory = self
                    .bases
                    .get("skills.dirs")
                    .unwrap_or(&self.load_options.workspace)
                    .join(&*directory);
            }
        }
        for (name, path) in [("data_dir", &mut raw.storage.data_dir)] {
            if !path.as_os_str().is_empty()
                && path.is_relative()
                && let Some(base) = self.bases.get(&format!("storage.{name}"))
            {
                *path = base.join(&*path);
            }
        }
        let mut extensions = Vec::new();
        for (id, extension) in raw.extensions {
            ensure!(
                valid_id(&id) && !extension.command.is_empty(),
                "extension entries need a valid id and a command"
            );
            validate_account(extension.account.as_deref(), &format!("extensions.{id}"))?;
            ensure!(
                extension
                    .env
                    .keys()
                    .all(|name| crate::provider::valid_extension_env_name(name)),
                "extensions.{id}.env names must be nonempty and contain no '=', ',' or NUL"
            );
            let mut cwd = extension.cwd;
            if let Some(path) = cwd.as_ref().filter(|path| path.is_relative())
                && let Some(base) = self.bases.get(&format!("extensions.{id}.cwd"))
            {
                cwd = Some(base.join(path));
            }
            if cwd.is_none() {
                cwd = self.origin_directory.clone();
            }
            extensions.push(ExtensionConfig {
                id,
                account: extension.account,
                command: extension.command,
                args: extension.args,
                cwd,
                env: extension.env,
            });
        }
        let endpoints = endpoints(&raw.providers)?;
        let configured_providers = raw
            .providers
            .keys()
            .filter(|name| {
                let prefix = format!("providers.{name}.");
                self.sources
                    .iter()
                    .any(|(path, source)| path.starts_with(&prefix) && source != "default")
            })
            .cloned()
            .collect();
        Ok(Settings {
            main_model: raw.model.main,
            main_provider: raw.model.provider,
            main_fallbacks: raw.model.fallbacks,
            agent: raw.agent,
            roles,
            endpoints,
            providers: raw.providers,
            context_strategy: strategy,
            memory_files: raw.context.memory_files,
            skills: raw.skills,
            compact_threshold_tokens: raw.context.compact_threshold_tokens,
            context_window_tokens: raw.context.window_tokens,
            auto_summarize: raw.context.auto_summarize,
            retry_base_ms: raw.context.retry_base_ms,
            verbatim_user_tokens: raw.context.classical.verbatim_user_tokens,
            lcm: raw.context.lcm,
            approvals: raw.approvals,
            agents: raw.agents,
            web: raw.web,
            storage: raw.storage,
            exec: raw.exec,
            quota: raw.quota,
            extensions,
            mcp: raw.mcp,
            delegate: raw.delegate,
            configured_providers,
        })
    }
    /// Valid TOML with dotted assignments and provenance. No environment value is read here.
    pub fn render(&self) -> Result<String> {
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
        for (name, path) in [("data_dir", &settings.storage.data_dir)] {
            merge_at(
                &mut effective,
                &["storage", name],
                Value::String(path.to_string_lossy().into_owned()),
            )?;
        }
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

#[derive(Clone)]
pub struct Settings {
    pub main_model: String,
    pub main_provider: String,
    pub main_fallbacks: Vec<String>,
    pub agent: RootAgentSettings,
    pub roles: ModelRoles,
    pub endpoints: ProviderEndpoints,
    pub providers: BTreeMap<String, ProviderSettings>,
    pub context_strategy: ContextStrategy,
    pub memory_files: bool,
    pub skills: SkillsConfig,
    pub compact_threshold_tokens: usize,
    pub context_window_tokens: Option<usize>,
    pub auto_summarize: bool,
    pub retry_base_ms: u64,
    pub verbatim_user_tokens: usize,
    pub lcm: crate::summarization::LcmConfig,
    pub approvals: ApprovalSettings,
    pub agents: Agents,
    pub web: WebSettings,
    pub storage: StorageSettings,
    pub exec: ExecConfig,
    pub quota: QuotaSettings,
    pub extensions: Vec<ExtensionConfig>,
    /// Live host servers; secrets are resolved at connection time only.
    pub mcp: McpSettings,
    pub delegate: DelegateSettings,
    configured_providers: BTreeSet<String>,
}
/// Skill directories add collections after project and user skills.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SkillsConfig {
    pub enabled: bool,
    pub dirs: Vec<PathBuf>,
}

/// Maximum automatic wait for a provider account's quota reset.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct QuotaSettings {
    /// Zero declines automatic waiting. It does not disable quota checks.
    pub max_wait_seconds: u64,
}
impl Default for QuotaSettings {
    fn default() -> Self {
        Self {
            max_wait_seconds: 21600,
        }
    }
}
impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dirs: Vec::new(),
        }
    }
}

/// Host limits for child sessions. `None` (unset) is unlimited. Zero depth
/// disables new children; zero concurrency or retention is rejected.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Agents {
    pub max_depth: Option<usize>,
    pub max_active_per_parent: Option<usize>,
    pub max_active_total: Option<usize>,
    pub max_retained: Option<usize>,
    /// Running-child inactivity interval. Zero disables host reminders.
    pub stale_reminder_secs: u64,
    /// Idle completion delivery; busy request notices remain available when off.
    pub auto_wake: bool,
    pub wake_debounce_ms: u64,
    /// Failed/cancelled/timed-out internal-worker chat retention.
    pub worker_retention_days: u32,
    /// Preserve every worker transcript, including acknowledged successes.
    pub worker_keep_all: bool,
}
impl Default for Agents {
    fn default() -> Self {
        Self {
            max_depth: None,
            max_active_per_parent: None,
            max_active_total: None,
            max_retained: None,
            stale_reminder_secs: 7200,
            auto_wake: true,
            wake_debounce_ms: 1500,
            worker_retention_days: 7,
            worker_keep_all: false,
        }
    }
}
impl Agents {
    fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("max_active_per_parent", self.max_active_per_parent),
            ("max_active_total", self.max_active_total),
            ("max_retained", self.max_retained),
        ] {
            if let Some(value) = value {
                ensure!(value > 0, "agents.{name} must be positive");
            }
        }
        Ok(())
    }
}
/// An empty path selects the workspace-scoped default; file paths resolve at their layer.
#[derive(Clone, Debug, Deserialize)]
pub struct StorageSettings {
    pub data_dir: PathBuf,
    pub finished_event_retention_hours: u32,
    pub telemetry_retention_days: u32,
    pub archive_after_days: u32,
    /// None keeps archived chats forever. Applied only when the host starts.
    pub archived_retention_days: Option<u32>,
    pub cold_tail_messages: u32,
    pub cold_min_messages: u32,
    pub cold_min_bytes: u64,
    pub cold_row_min_bytes: u32,
}
/// Command execution limits; see docs/exec.md. An unset cap means no limit.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct ExecConfig {
    /// Sync `exec` calls move to background after this many seconds; 0 backgrounds at once.
    pub sync_timeout_secs: u64,
    pub max_jobs_per_session: Option<usize>,
    pub max_jobs: Option<usize>,
    /// Per-job output spool cap in bytes.
    pub max_output_bytes: usize,
}
impl Default for ExecConfig {
    fn default() -> Self {
        Self {
            sync_timeout_secs: 300,
            max_jobs_per_session: None,
            max_jobs: None,
            max_output_bytes: 8 * 1024 * 1024,
        }
    }
}
impl ExecConfig {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.max_output_bytes > 0,
            "exec.max_output_bytes must be positive"
        );
        ensure!(
            self.max_jobs_per_session != Some(0),
            "exec.max_jobs_per_session must be positive; leave it unset for no limit"
        );
        ensure!(
            self.max_jobs != Some(0),
            "exec.max_jobs must be positive; leave it unset for no limit"
        );
        Ok(())
    }
}
impl Settings {
    pub fn root_storage_policy(&self) -> bubblepaw_core::RootStoragePolicy {
        bubblepaw_core::RootStoragePolicy {
            store: bubblepaw_core::StoreOptions {
                finished_event_retention_hours: self.storage.finished_event_retention_hours,
                telemetry_retention_days: self.storage.telemetry_retention_days,
                archive_after_days: self.storage.archive_after_days,
                cold_tail_messages: self.storage.cold_tail_messages,
                cold_min_messages: self.storage.cold_min_messages,
                cold_min_bytes: self.storage.cold_min_bytes,
                cold_row_min_bytes: self.storage.cold_row_min_bytes,
            },
            archived_retention_days: self.storage.archived_retention_days,
        }
    }

    /// Configured route names mapped to their quota and usage account identity.
    /// Aliases share state only when explicitly assigned the same account.
    pub fn provider_accounts(&self) -> BTreeMap<String, String> {
        self.providers
            .iter()
            .map(|(name, provider)| {
                (
                    name.clone(),
                    provider.account.as_ref().unwrap_or(name).clone(),
                )
            })
            .chain(self.extensions.iter().map(|extension| {
                (
                    extension.id.clone(),
                    extension.account.as_ref().unwrap_or(&extension.id).clone(),
                )
            }))
            .collect()
    }
    /// Shared startup/apply checks, before any provider credential is read.
    pub fn validate_provider_setup(&self) -> Result<()> {
        for (name, provider) in &self.providers {
            provider.validate(name)?;
            validate_provider_setup(provider, &self.web.provider_key_envs)?;
        }
        Ok(())
    }
    /// Read keys only at transport registration, after configuration is validated.
    pub fn register_providers(&self, registry: &mut ProviderRegistry) -> Result<()> {
        self.register_providers_with_env(registry, &|name| std::env::var(name).ok())
    }

    pub fn register_providers_with_env(
        &self,
        registry: &mut ProviderRegistry,
        environment: &dyn Fn(&str) -> Option<String>,
    ) -> Result<()> {
        for (name, provider) in &self.providers {
            // The historical built-in main transports were only registered when
            // selected. Configured aliases opt into additional transports.
            if matches!(name.as_str(), "codex" | "api" | "demo")
                && !self.configured_providers.contains(name)
            {
                continue;
            }
            let kind = provider.kind.as_deref().unwrap_or("chat-completions");
            if matches!(kind, "codex" | "scripted") && registry.get(name).is_some() {
                continue;
            }
            if kind == "scripted" {
                registry.insert(
                    name,
                    Arc::new(crate::provider::ScriptedProvider::demo(
                        !self.approvals.read_only,
                    )),
                )?;
                continue;
            }
            let Some(base) = provider
                .base_url
                .as_ref()
                .filter(|url| !url.trim().is_empty())
            else {
                continue;
            };
            if kind == "codex" {
                let auth = provider
                    .auth_file
                    .clone()
                    .or_else(|| {
                        environment("CODEX_HOME").map(|path| PathBuf::from(path).join("auth.json"))
                    })
                    .or_else(|| {
                        environment("USERPROFILE")
                            .or_else(|| environment("HOME"))
                            .map(|path| PathBuf::from(path).join(".codex/auth.json"))
                    });
                if let Some(auth) = auth.filter(|path| path.is_file()) {
                    let mut adapter = ResponsesProvider::codex_auth(auth, base.clone())?;
                    if let Some(effort) = provider
                        .reasoning
                        .get("reasoning_effort")
                        .and_then(Value::as_str)
                    {
                        adapter = adapter.with_reasoning_effort(effort);
                    }
                    registry.insert(name, Arc::new(adapter))?;
                }
                continue;
            }
            if kind == "chat-completions" && provider.key_env.is_none() {
                let adapter = ChatCompletionsProvider::without_key(base.clone())?
                    .with_reasoning_fields(serde_json::to_value(&provider.reasoning)?)?
                    .with_reasoning_replay(provider.reasoning_replay())?;
                if registry.get(name).is_none() {
                    registry.insert(name, Arc::new(adapter))?;
                }
                continue;
            }
            let Some(key) = provider
                .key_env
                .as_ref()
                .and_then(|name| environment(name))
                .filter(|key| !key.is_empty())
            else {
                continue;
            };
            ensure!(
                reqwest::header::HeaderValue::from_str(&key).is_ok(),
                "Invalid credential header for provider {name}"
            );
            match kind {
                "anthropic-messages" if registry.get(name).is_none() => {
                    let mut adapter = MessagesProvider::api_key(key, base.clone())?;
                    if is_opencode_route(name) {
                        adapter = adapter.with_opencode_route();
                    }
                    registry.insert(name, Arc::new(adapter))?;
                }
                "chat-completions" if registry.get(name).is_none() => {
                    let mut adapter = ChatCompletionsProvider::api_key(key, base.clone())?
                        .with_reasoning_fields(serde_json::to_value(&provider.reasoning)?)?
                        .with_reasoning_replay(provider.reasoning_replay())?;
                    if is_opencode_route(name) {
                        adapter = adapter.with_opencode_route();
                    }
                    registry.insert(name, Arc::new(adapter))?;
                }
                "responses" if registry.get(name).is_none() => {
                    let mut adapter = ResponsesProvider::api_key(key, base.clone())?;
                    if is_opencode_route(name) {
                        adapter = adapter.with_opencode_route();
                    }
                    if let Some(effort) = provider
                        .reasoning
                        .get("reasoning_effort")
                        .and_then(Value::as_str)
                    {
                        adapter = adapter.with_reasoning_effort(effort);
                    }
                    registry.insert(name, Arc::new(adapter))?;
                }
                "decisions" | "openai-decisions" if registry.get_decision(name).is_none() => {
                    registry.insert_decision(
                        name,
                        Arc::new(OpenAiDecisions::new(key, base.clone())?),
                    )?;
                }
                "openrouter-decisions" if registry.get_decision(name).is_none() => {
                    registry.insert_decision(
                        name,
                        Arc::new(OpenRouterDecisions::new(key, base.clone())?),
                    )?;
                }
                "opencode-decisions" if registry.get_decision(name).is_none() => {
                    registry.insert_decision(
                        name,
                        Arc::new(OpenCodeDecisions::new(key, base.clone())?),
                    )?;
                }
                _ => {}
            }
        }
        Ok(())
    }
}
/// HTTPS is required except for a machine-local transport fixture or relay.
/// An explicitly empty URL disables a provider and is intentionally accepted.
pub fn validate_provider_setup(
    provider: &ProviderSettings,
    allowed_key_envs: &[String],
) -> Result<()> {
    if let Some(base) = provider
        .base_url
        .as_deref()
        .filter(|base| !base.trim().is_empty())
    {
        let url =
            reqwest::Url::parse(base).map_err(|_| anyhow::anyhow!("Invalid provider base_url"))?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "provider base_url must be HTTP(S) without credentials/query/fragment"
        );
        let loopback = url.host_str().is_some_and(|host| {
            host == "localhost"
                || host.ends_with(".localhost")
                || host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        });
        ensure!(
            url.scheme() == "https" || loopback,
            "base_url must use https unless it points at this machine (localhost/127.0.0.1/::1)"
        );
    }
    if let Some(name) = &provider.key_env {
        ensure!(
            (1..=128).contains(&name.len())
                && name
                    .bytes()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
                && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'),
            "Invalid provider key_env name"
        );
        ensure!(
            ["_API_KEY", "_KEY", "_TOKEN"]
                .iter()
                .any(|suffix| name.ends_with(suffix))
                || allowed_key_envs.contains(name),
            "key_env {name} is not an allowed key variable; names must end in _API_KEY, _KEY or _TOKEN, or be listed in [web] provider_key_envs"
        );
    }
    Ok(())
}

/// A known LLM/extension prefix selects its route. Other slash-containing main
/// model IDs retain the legacy vendor namespace on the default provider.
pub fn selected_main_provider(settings: &Settings) -> &str {
    if let Some((id, _)) = settings.main_model.split_once('/')
        && (settings
            .extensions
            .iter()
            .any(|extension| extension.id == id)
            || settings.providers.get(id).is_some_and(|provider| {
                matches!(
                    provider.kind.as_deref().unwrap_or("chat-completions"),
                    "codex" | "scripted" | "responses" | "chat-completions" | "anthropic-messages"
                )
            }))
    {
        return id;
    }
    &settings.main_provider
}
/// `bubblepaw delegate` model classes. Categories, the explore share and other
/// delegation policy are constants in the delegate module, not configuration.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegateSettings {
    #[serde(default)]
    pub classes: BTreeMap<String, ModelClass>,
}
/// One model class: its default and the other candidates, each `provider/model`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelClass {
    pub default: String,
    #[serde(default)]
    pub candidates: Vec<String>,
}
impl DelegateSettings {
    fn validate(&self) -> Result<()> {
        for (name, class) in &self.classes {
            for model in std::iter::once(&class.default).chain(&class.candidates) {
                ensure!(
                    model
                        .split_once('/')
                        .is_some_and(|(p, m)| !p.is_empty() && !m.is_empty()),
                    "delegate.classes.{name} models must be provider/model"
                );
            }
        }
        Ok(())
    }
}
/// File-safe web settings: only the key's environment name is stored/rendered.
#[derive(Clone, Debug, Deserialize)]
pub struct WebSettings {
    pub enabled: bool,
    /// Ordered backends; a single string is a one-entry list.
    #[serde(deserialize_with = "one_or_many")]
    pub search: Vec<String>,
    #[serde(deserialize_with = "one_or_many")]
    pub fetch_backend: Vec<String>,
    pub key_env: String,
    pub deepseek_key_env: String,
    pub deepseek_base_url: String,
    /// Additional key variable names permitted by web provider setup.
    pub provider_key_envs: Vec<String>,
    pub allow_private: bool,
    pub user_agent: String,
    pub cache_ttl_secs: u64,
    pub cache_max_bytes: usize,
    #[serde(default)]
    pub max_searches: Option<usize>,
    #[serde(default)]
    pub max_fetches: Option<usize>,
    pub cooldown_secs: u64,
    pub expose: WebExpose,
}
impl WebSettings {
    fn search_backends(&self, api_key: &str, deepseek_key: &str) -> Result<Vec<SearchBackend>> {
        ensure!(
            !self.search.is_empty(),
            "web.search must list at least one backend"
        );
        self.search
            .iter()
            .map(|value| match value.as_str() {
                "ddg" => Ok(SearchBackend::DuckDuckGo),
                "deepseek" => Ok(SearchBackend::DeepSeek {
                    base_url: self.deepseek_base_url.clone(),
                    api_key: deepseek_key.to_owned(),
                }),
                "brave" => Ok(SearchBackend::Brave {
                    api_key: api_key.to_owned(),
                }),
                value if value.starts_with("searxng:") => Ok(SearchBackend::SearxNg {
                    url: value[8..].to_owned(),
                }),
                _ => bail!("web.search must be deepseek, ddg, searxng:<url> or brave"),
            })
            .collect()
    }
    fn fetch_backends(&self) -> Result<Vec<FetchBackend>> {
        ensure!(
            !self.fetch_backend.is_empty(),
            "web.fetch_backend must list at least one backend"
        );
        self.fetch_backend
            .iter()
            .map(|value| match value.as_str() {
                "direct" => Ok(FetchBackend::Direct),
                value => match value.strip_prefix("crawl4ai:") {
                    Some(url) => Ok(FetchBackend::Crawl4Ai {
                        url: url.to_owned(),
                    }),
                    None => bail!("web.fetch_backend must be direct or crawl4ai:<url>"),
                },
            })
            .collect()
    }
    fn config(&self, api_key: String, deepseek_key: String) -> Result<WebConfig> {
        Ok(WebConfig {
            search: self.search_backends(&api_key, &deepseek_key)?,
            fetch: self.fetch_backends()?,
            expose: self.expose.clone(),
            allow_private: self.allow_private,
            user_agent: self.user_agent.clone(),
            cache_ttl_secs: self.cache_ttl_secs,
            cache_max_bytes: self.cache_max_bytes,
            max_searches: self.max_searches,
            max_fetches: self.max_fetches,
            cooldown_secs: self.cooldown_secs,
        })
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.key_env.is_empty() && !self.key_env.contains('=') && !self.key_env.contains('\0'),
            "web.key_env must be an environment variable name"
        );
        ensure!(
            !self.deepseek_key_env.is_empty()
                && !self.deepseek_key_env.contains('=')
                && !self.deepseek_key_env.contains('\0'),
            "web.deepseek_key_env must be an environment variable name"
        );
        // Validate URL/header/limits without reading credentials, including when disabled.
        crate::net::NetClient::new(&self.config(String::new(), String::new())?)?;
        Ok(())
    }
    /// Resolve secrets only when an enabled runtime constructs its web tools.
    pub fn net_config(&self) -> Result<Option<WebConfig>> {
        self.net_config_with_env(&|name| std::env::var(name).ok())
    }

    pub fn net_config_with_env(
        &self,
        environment: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Option<WebConfig>> {
        self.validate()?;
        if !self.enabled {
            return Ok(None);
        }
        // Missing or empty search keys are not errors: their backends skip when called.
        let key = if self.search.iter().any(|value| value == "brave") {
            environment(&self.key_env).unwrap_or_default()
        } else {
            String::new()
        };
        let deepseek_key = if self.search.iter().any(|value| value == "deepseek") {
            environment(&self.deepseek_key_env).unwrap_or_default()
        } else {
            String::new()
        };
        Ok(Some(self.config(key, deepseek_key)?))
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ApprovalSettings {
    pub confirm: String,
    #[serde(default)]
    pub tools: BTreeMap<String, String>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub read_only: bool,
    /// The fixed exec blocklist (docs/approvals.md). On unless a config turns it off.
    #[serde(default = "default_hardline")]
    pub hardline: bool,
    /// Approver prompt template; None uses `bubblepaw_core::DEFAULT_AUTO_PROMPT`.
    pub auto_prompt: Option<String>,
}
fn default_hardline() -> bool {
    true
}
#[derive(Clone, Debug, Deserialize)]
pub struct ProviderSettings {
    pub kind: Option<String>,
    /// Stable quota identity; omission uses this provider entry's name.
    pub account: Option<String>,
    pub base_url: Option<String>,
    pub key_env: Option<String>,
    pub auth_file: Option<PathBuf>,
    #[serde(default)]
    pub reasoning: BTreeMap<String, Value>,
    /// Per-model settings. Model IDs may contain dots, so they are values here,
    /// not table keys.
    #[serde(default)]
    pub models: Vec<ProviderModelSettings>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderModelSettings {
    pub id: String,
    /// Explicit selected-model override; HTTP transports otherwise accept images.
    #[serde(default)]
    pub image_input: Option<bool>,
    /// Chat-completions only: which stored `reasoning_content` goes back on wire.
    #[serde(default)]
    pub reasoning_replay: Option<ReasoningReplay>,
}
impl ProviderSettings {
    /// Exact model ID to an explicit chat replay policy; omissions use `tool_loop`.
    pub fn reasoning_replay(&self) -> BTreeMap<String, ReasoningReplay> {
        self.models
            .iter()
            .filter_map(|model| {
                model
                    .reasoning_replay
                    .map(|policy| (model.id.clone(), policy))
            })
            .collect()
    }
    pub fn validate(&self, name: &str) -> Result<()> {
        ensure!(valid_id(name), "Invalid provider id: providers.{name}");
        validate_account(self.account.as_deref(), &format!("providers.{name}"))?;
        let kind = self.kind.as_deref().unwrap_or("chat-completions");
        ensure!(
            !matches!(
                kind,
                "decisions" | "openai-decisions" | "openrouter-decisions" | "opencode-decisions"
            ) || name != "llm",
            "Invalid decision provider id: providers.{name}"
        );
        ensure!(
            matches!(
                kind,
                "codex"
                    | "scripted"
                    | "responses"
                    | "chat-completions"
                    | "anthropic-messages"
                    | "decisions"
                    | "openai-decisions"
                    | "openrouter-decisions"
                    | "opencode-decisions"
            ),
            "Invalid config value: providers.{name}.kind"
        );
        if let Some(base) = self
            .base_url
            .as_ref()
            .filter(|base| !base.trim().is_empty())
        {
            let parsed = reqwest::Url::parse(base)
                .map_err(|_| anyhow::anyhow!("Invalid config value: providers.{name}.base_url"))?;
            ensure!(
                matches!(parsed.scheme(), "http" | "https")
                    && parsed.username().is_empty()
                    && parsed.password().is_none()
                    && parsed.query().is_none()
                    && parsed.fragment().is_none(),
                "Invalid config value: providers.{name}.base_url"
            );
        }
        if let Some(key) = &self.key_env {
            ensure!(
                !key.is_empty() && !key.contains(['=', '\0']),
                "Invalid config value: providers.{name}.key_env"
            );
        }
        if let Some(effort) = self.reasoning.get("reasoning_effort") {
            ensure!(
                effort.as_str().is_some_and(|effort| {
                    serde_json::from_value::<ReasoningEffort>(serde_json::json!(effort)).is_ok()
                }),
                "Invalid config value: providers.{name}.reasoning.reasoning_effort"
            );
        }
        if let Some(thinking) = self.reasoning.get("thinking") {
            let kind = thinking
                .as_str()
                .or_else(|| thinking.get("type").and_then(Value::as_str));
            ensure!(
                matches!(kind, Some("enabled" | "adaptive")),
                "Invalid config value: providers.{name}.reasoning.thinking; thinking must remain enabled"
            );
        }
        let mut ids = BTreeSet::new();
        for model in &self.models {
            ensure!(
                !model.id.is_empty()
                    && !model
                        .id
                        .chars()
                        .any(|c| c.is_whitespace() || c.is_control())
                    && ids.insert(model.id.as_str()),
                "Invalid config value: providers.{name}.models.id must be a unique exact model ID"
            );
        }
        ensure!(
            kind == "chat-completions"
                || self.models.iter().all(|model| model
                    .reasoning_replay
                    .is_none_or(|policy| policy == ReasoningReplay::None)),
            "providers.{name}.models.reasoning_replay applies only to chat-completions; Responses and Messages replay only to their origin provider and model"
        );
        ensure!(
            kind != "anthropic-messages" || self.reasoning.is_empty(),
            "providers.{name}.reasoning is not configurable for anthropic-messages; set roles.<role>.reasoning_effort"
        );
        if !matches!(kind, "codex" | "scripted") {
            ensure!(
                self.base_url.is_some()
                    && (self.key_env.is_some()
                        || (kind == "chat-completions"
                            && self
                                .base_url
                                .as_deref()
                                .is_some_and(crate::provider::is_loopback_url))),
                "providers.{name} needs base_url and key_env"
            );
        }
        Ok(())
    }
}
fn valid_id(name: &str) -> bool {
    !name.is_empty() && !name.contains(['/', '.']) && !name.chars().any(char::is_whitespace)
}
fn validate_account(account: Option<&str>, path: &str) -> Result<()> {
    ensure!(
        account.is_none_or(|account| !account.is_empty()
            && account.len() <= 256
            && account.trim() == account
            && !account.chars().any(char::is_control)),
        "{path}.account must contain 1..=256 bytes without surrounding whitespace or control characters"
    );
    Ok(())
}
/// Root harness settings, resolved at host startup. None retains the runner's instructions.
#[derive(Clone, Debug, Deserialize)]
pub struct RootAgentSettings {
    pub system_prompt: Option<String>,
    pub max_rounds: Option<usize>,
    #[serde(default)]
    pub execution_mode: ExecutionMode,
}
#[derive(Deserialize)]
struct RawSettings {
    model: ModelSettings,
    agent: RootAgentSettings,
    storage: StorageSettings,
    exec: ExecConfig,
    quota: QuotaSettings,
    providers: BTreeMap<String, ProviderSettings>,
    roles: BTreeMap<String, RoleEntry>,
    context: ContextSettings,
    skills: SkillsConfig,
    approvals: ApprovalSettings,
    agents: Agents,
    web: WebSettings,
    extensions: BTreeMap<String, ExtensionSettings>,
    mcp: McpSettings,
    delegate: DelegateSettings,
}
#[derive(Deserialize)]
struct ModelSettings {
    main: String,
    provider: String,
    #[serde(default)]
    fallbacks: Vec<String>,
}
#[derive(Deserialize)]
struct ContextSettings {
    strategy: String,
    memory_files: bool,
    compact_threshold_tokens: usize,
    window_tokens: Option<usize>,
    auto_summarize: bool,
    retry_base_ms: u64,
    classical: ClassicalSettings,
    #[serde(default)]
    lcm: crate::summarization::LcmConfig,
}
#[derive(Deserialize)]
struct ClassicalSettings {
    verbatim_user_tokens: usize,
}
#[derive(Deserialize)]
struct ExtensionSettings {
    command: String,
    account: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    cwd: Option<PathBuf>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}
#[derive(Clone, Deserialize)]
#[serde(untagged)]
enum RoleEntry {
    Model(String),
    Models(Vec<String>),
    Table(RoleTable),
}
#[derive(Clone, Deserialize)]
struct RoleTable {
    models: Vec<String>,
    reasoning_effort: Option<ReasoningEffort>,
    max_output_tokens: Option<u32>,
    repeats: Option<usize>,
    threshold: Option<f64>,
}
impl RoleEntry {
    fn models(&self) -> Vec<String> {
        match self {
            Self::Model(model) => vec![model.clone()],
            Self::Models(models) => models.clone(),
            Self::Table(entry) => entry.models.clone(),
        }
    }
}

fn endpoints(providers: &BTreeMap<String, ProviderSettings>) -> Result<ProviderEndpoints> {
    let mut result = ProviderEndpoints::default();
    if let Some(provider) = providers.get("opencode") {
        result.opencode_base_url = provider
            .base_url
            .clone()
            .filter(|url| !url.trim().is_empty());
        if let Some(key) = &provider.key_env {
            result.opencode_key_env = key.clone();
        }
        result.opencode_reasoning_fields = serde_json::to_value(&provider.reasoning)?;
        result.opencode_reasoning_replay = provider.reasoning_replay();
    }
    if let Some(provider) = providers.get("opencode-messages") {
        result.opencode_messages_base_url = provider
            .base_url
            .clone()
            .filter(|url| !url.trim().is_empty());
        if let Some(key) = &provider.key_env {
            result.opencode_messages_key_env = key.clone();
        }
    }
    if let Some(provider) = providers.get("openai") {
        if let Some(url) = &provider.base_url {
            result.openai_base_url = url.clone();
        }
        if let Some(key) = &provider.key_env {
            result.openai_key_env = key.clone();
        }
    }
    if let Some(provider) = providers.get("openrouter") {
        if let Some(url) = &provider.base_url {
            result.openrouter_decisions_url = url.clone();
        }
        if let Some(key) = &provider.key_env {
            result.openrouter_key_env = key.clone();
        }
    }
    // Individual providers validate empty URLs as explicitly disabled.
    if !result.openai_base_url.is_empty() && !result.openrouter_decisions_url.is_empty() {
        result.validate()?;
    }
    Ok(result)
}
pub fn parse_value(value: &str) -> Value {
    format!("value = {value}")
        .parse::<Value>()
        .ok()
        .and_then(|mut table| {
            let table = table.as_table_mut()?;
            (table.len() == 1).then(|| table.remove("value"))?
        })
        .unwrap_or_else(|| Value::String(value.into()))
}
/// Shape checks for `[mcp]` before typed deserialization. Secrets are never read
/// here: `env_from` entries are checked as environment names only.
fn validate_mcp(mcp: Option<&Value>) -> Result<()> {
    let settings: McpSettings = mcp
        .cloned()
        .unwrap_or_else(|| Value::Table(toml::map::Map::new()))
        .try_into()
        .map_err(|_| anyhow::anyhow!("Invalid config value: mcp; invalid field type"))?;
    // Validate the allowlist even when no servers are configured.
    let probe = bubblepaw_mcp::McpServerSettings {
        command: "validation-only".into(),
        ..Default::default()
    };
    probe.validate(&settings.header_key_envs).map_err(|_| {
        anyhow::anyhow!(
            "Invalid config value: mcp.header_key_envs; expected environment variable identifiers"
        )
    })?;
    let Some(servers) = mcp.and_then(|mcp| mcp.get("servers")) else {
        return Ok(());
    };
    let servers = servers.as_table().context("mcp.servers must be a table")?;
    for (name, server) in servers {
        ensure!(valid_id(name), "Invalid config value: mcp.servers.{name}");
        let server = server
            .as_table()
            .with_context(|| format!("Invalid config value: mcp.servers.{name}"))?;
        settings.servers[name]
            .validate(&settings.header_key_envs)
            .with_context(|| format!("Invalid config value: mcp.servers.{name}"))?;
        for key in ["startup_timeout_secs", "call_timeout_secs"] {
            if let Some(seconds) = server.get(key) {
                ensure!(
                    seconds.as_integer().is_some_and(|seconds| seconds > 0),
                    "Invalid config value: mcp.servers.{name}.{key}; must be positive"
                );
            }
        }
        validate_mcp_approval(
            server.get("approval"),
            &format!("mcp.servers.{name}.approval"),
        )?;
        if let Some(env) = server.get("env").and_then(Value::as_table) {
            for key in env.keys() {
                ensure!(
                    env_name(key),
                    "Invalid config value: mcp.servers.{name}.env"
                );
            }
        }
        if let Some(env_from) = server.get("env_from").and_then(Value::as_table) {
            // The value is the host variable's name; its secret is read at spawn time.
            for (key, value) in env_from {
                ensure!(
                    env_name(key) && value.as_str().is_some_and(env_name),
                    "Invalid config value: mcp.servers.{name}.env_from; expected environment variable names"
                );
            }
        }
        if let Some(tools) = server.get("tools") {
            let tools = tools
                .as_table()
                .with_context(|| format!("Invalid config value: mcp.servers.{name}.tools"))?;
            for (tool, settings) in tools {
                if let Some(settings) = settings.as_table() {
                    validate_mcp_approval(
                        settings.get("approval"),
                        &format!("mcp.servers.{name}.tools.{tool}.approval"),
                    )?;
                }
            }
        }
    }
    Ok(())
}
fn validate_mcp_approval(value: Option<&Value>, path: &str) -> Result<()> {
    if let Some(value) = value {
        ensure!(
            matches!(value.as_str(), Some("confirm" | "none")),
            "Invalid config value: {path}; expected confirm or none"
        );
    }
    Ok(())
}
fn env_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['=', '\0'])
}
fn validate_keys(value: &Value, path: &str) -> Result<()> {
    ensure!(
        path != "agent.enable_js",
        "agent.enable_js was removed; use execution_mode = \"js\" in [agent] to select JavaScript mode"
    );
    if !path.is_empty() {
        ensure!(known_path(path), "Unknown config key: {path}");
    }
    if let Value::Table(table) = value {
        // Dots are valid HTTP token characters, not nested configuration keys.
        let header_table = matches!(
            path.split('.').collect::<Vec<_>>().as_slice(),
            ["mcp", "servers", _, "headers_from"]
        );
        for (key, value) in table {
            ensure!(
                !key.is_empty() && (header_table || !key.contains('.')),
                "Unknown config key: {}",
                join_path(path, key)
            );
            validate_keys(value, &join_path(path, key))?;
        }
    }
    Ok(())
}
fn join_path(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.into()
    } else {
        format!("{path}.{key}")
    }
}
fn merge_at(target: &mut Value, parts: &[&str], value: Value) -> Result<()> {
    if let Some((part, rest)) = parts.split_first() {
        if !target.is_table() {
            *target = Value::Table(toml::map::Map::new());
        }
        let table = target.as_table_mut().unwrap();
        let entry = table
            .entry((*part).to_owned())
            .or_insert_with(|| Value::Table(toml::map::Map::new()));
        merge_at(entry, rest, value)
    } else {
        match (target, value) {
            (Value::Table(target), Value::Table(value)) if !value.is_empty() => {
                for (key, value) in value {
                    merge_at(
                        target
                            .entry(key)
                            .or_insert_with(|| Value::Table(toml::map::Map::new())),
                        &[],
                        value,
                    )?;
                }
            }
            (target, value) => *target = value,
        }
        Ok(())
    }
}
fn normalize_roles(value: &mut Value, path: &str) {
    if matches!(
        path,
        "roles.summary" | "roles.chapter" | "roles.decider" | "roles.approver"
    ) && !value.is_table()
    {
        let models = if value.is_array() {
            value.clone()
        } else {
            Value::Array(vec![value.clone()])
        };
        let mut table = toml::map::Map::new();
        table.insert("models".into(), models);
        *value = Value::Table(table);
    } else if let Value::Table(table) = value {
        for (key, value) in table {
            normalize_roles(value, &join_path(path, key));
        }
    }
}
fn collect_sources(
    value: &Value,
    path: &str,
    source: &str,
    sources: &mut BTreeMap<String, String>,
) {
    if let Value::Table(table) = value
        && !table.is_empty()
    {
        for (key, value) in table {
            collect_sources(value, &join_path(path, key), source, sources);
        }
    } else {
        sources.insert(path.into(), source.into());
    }
}
fn record_override(
    value: &Value,
    path: &str,
    source: &str,
    base: Option<&Path>,
    sources: &mut BTreeMap<String, String>,
    bases: &mut BTreeMap<String, PathBuf>,
) {
    if let Value::Table(table) = value
        && !table.is_empty()
    {
        sources.remove(path);
        for (key, value) in table {
            record_override(value, &join_path(path, key), source, base, sources, bases);
        }
    } else {
        sources.retain(|key, _| key != path && !key.starts_with(&format!("{path}.")));
        bases.retain(|key, _| key != path && !key.starts_with(&format!("{path}.")));
        sources.insert(path.into(), source.into());
        if let Some(base) = base {
            bases.insert(path.into(), base.to_path_buf());
        }
    }
}
fn render_leaves(
    value: &Value,
    parts: &mut Vec<String>,
    sources: &BTreeMap<String, String>,
    output: &mut String,
) {
    if let Value::Table(table) = value
        && !table.is_empty()
    {
        for (key, value) in table {
            parts.push(key.clone());
            render_leaves(value, parts, sources, output);
            parts.pop();
        }
    } else {
        let path = parts.join(".");
        let source = sources
            .get(&path)
            .map(String::as_str)
            .unwrap_or("default")
            .replace(['\r', '\n'], " ");
        output.push_str(&format!(
            "# source: {source}\n{} = {value}\n",
            parts
                .iter()
                .map(|part| Value::String(part.clone()).to_string())
                .collect::<Vec<_>>()
                .join(".")
        ));
    }
}
fn key_names(value: &Value) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    collect_key_names(value, "", &mut names);
    names
}
fn collect_key_names(value: &Value, path: &str, names: &mut BTreeSet<String>) {
    if (path.ends_with(".key_env") || path == "web.deepseek_key_env")
        && let Some(name) = value.as_str()
    {
        names.insert(name.into());
    }
    // An env_from value names a host secret; it is exempt from generic overrides like key_env.
    let parts: Vec<_> = path.split('.').collect();
    if matches!(
        parts.as_slice(),
        ["mcp", "servers", _, "env_from", _] | ["mcp", "servers", _, "headers_from", _, ..]
    ) && let Some(name) = value.as_str()
    {
        names.insert(name.into());
    }
    if let Value::Table(table) = value {
        for (key, value) in table {
            collect_key_names(value, &join_path(path, key), names);
        }
    }
}

/// Whether a configured provider alias is an OpenCode Go route. Only these
/// get the `x-opencode-session` header. The alias decides, not the URL: the
/// same wire kind serves OpenCode and plain provider hosts alike.
pub fn is_opencode_route(name: &str) -> bool {
    name == "opencode" || name.starts_with("opencode-")
}

#[cfg(test)]
#[path = "config_mcp_tests.rs"]
mod mcp_tests;

#[cfg(test)]
mod tests {
    #[test]
    fn attach_config_keeps_layers_and_only_referenced_environment() {
        let root = tempfile::tempdir().unwrap();
        let mut options = options(root.path());
        options.env = vec![
            ("CODEX_HOME".into(), "credentials".into()),
            ("OPENAI_API_KEY".into(), "private-client-credential".into()),
            ("BP_FAKE_STEP_LOG".into(), "client-log".into()),
            ("ATTACH_MCP_TOKEN".into(), "private-mcp-credential".into()),
            ("CRAWL4AI_API_TOKEN".into(), "client-crawl-token".into()),
            ("BUBBLEPAW_STORAGE__DATA_DIR".into(), "records".into()),
        ];
        options
            .cli
            .push(Override::set("skills.dirs=['skills']").unwrap());
        options.cli.push(
            Override::set("web.fetch_backend=['direct','crawl4ai:http://127.0.0.1:11235']")
                .unwrap(),
        );
        options
            .cli
            .push(Override::set("mcp.servers.fixture.command='fixture'").unwrap());
        options
            .cli
            .push(Override::set("mcp.servers.fixture.env_from.TOKEN='ATTACH_MCP_TOKEN'").unwrap());
        // A provider credential that an MCP server also references.
        options
            .cli
            .push(Override::set("mcp.servers.fixture.env_from.SHARED='OPENAI_API_KEY'").unwrap());
        let mut original = EffectiveConfig::load(options).unwrap();
        original.anchor_paths(root.path());
        // Even an accidental serialization before preparation cannot send the
        // full local snapshot. The explicit transport snapshot is allowlisted.
        assert!(
            !serde_json::to_string(&original)
                .unwrap()
                .contains("client-log")
        );
        original.prepare_for_attach();
        let wire = serde_json::to_string(&original).unwrap();
        assert!(!wire.contains("client-log"));
        assert!(!wire.contains("CODEX_HOME"));
        let snapshot: EffectiveConfig =
            serde_json::from_slice(&serde_json::to_vec(&original).unwrap()).unwrap();
        for config in [snapshot.clone(), snapshot.reload().unwrap()] {
            let settings = config.settings().unwrap();
            assert_eq!(settings.storage.data_dir, root.path().join("records"));
            assert_eq!(settings.skills.dirs, vec![root.path().join("skills")]);
            assert_eq!(config.environment("CODEX_HOME"), None);
            assert_eq!(
                config.environment("CRAWL4AI_API_TOKEN").as_deref(),
                Some("client-crawl-token")
            );
            assert_eq!(
                settings.providers["codex"].auth_file,
                Some(root.path().join("credentials/auth.json"))
            );
            assert_eq!(
                config.codex_models_cache(),
                Some(root.path().join("credentials/models_cache.json"))
            );
            assert_eq!(config.environment("UNSET_CLIENT_CREDENTIAL"), None);
            assert_eq!(
                config.environment("OPENAI_API_KEY").as_deref(),
                Some("private-client-credential")
            );
            assert_eq!(config.environment("BP_FAKE_STEP_LOG").as_deref(), None);
            assert_eq!(
                config.mcp_environment(),
                vec![
                    ("ATTACH_MCP_TOKEN".into(), "private-mcp-credential".into()),
                    ("OPENAI_API_KEY".into(), "private-client-credential".into()),
                ]
            );
            let runtime = config.runtime_environment();
            assert!(
                !runtime
                    .iter()
                    .any(|(_, value)| value == "private-mcp-credential")
            );
            // Also referenced by a provider, so subprocesses see it as standalone ones would.
            assert!(runtime.iter().any(|(name, value)| {
                name == "OPENAI_API_KEY" && value == "private-client-credential"
            }));
            assert!(!format!("{config:?}").contains("private-client-credential"));
            assert!(
                !config
                    .render()
                    .unwrap()
                    .contains("private-client-credential")
            );
            assert!(
                !config
                    .load_options()
                    .env
                    .iter()
                    .any(|(name, _)| name == "OPENAI_API_KEY")
            );
        }
    }

    #[test]
    fn only_opencode_aliases_are_opencode_routes() {
        use super::is_opencode_route;
        assert!(is_opencode_route("opencode"));
        assert!(is_opencode_route("opencode-messages"));
        assert!(!is_opencode_route("api"));
        assert!(!is_opencode_route("anthropic"));
        assert!(!is_opencode_route("openai"));
        assert!(!is_opencode_route("codex"));
    }

    use super::*;
    #[test]
    fn agents_defaults_and_layers_preserve_sources() {
        let root = tempfile::tempdir().unwrap();
        let defaults = EffectiveConfig::load(options(root.path())).unwrap();
        assert_eq!(
            defaults.settings().unwrap().agents,
            Agents {
                max_depth: None,
                max_active_per_parent: None,
                max_active_total: None,
                max_retained: None,
                stale_reminder_secs: 7200,
                auto_wake: true,
                wake_debounce_ms: 1500,
                worker_retention_days: 7,
                worker_keep_all: false,
            }
        );
        let rendered: Value = defaults.render().unwrap().parse().unwrap();
        assert_eq!(
            rendered["agents"]["stale_reminder_secs"].as_integer(),
            Some(7200)
        );
        assert_eq!(
            defaults.source("agents.stale_reminder_secs"),
            Some("default")
        );
        assert_eq!(
            rendered["agents"]["worker_retention_days"].as_integer(),
            Some(7)
        );
        assert_eq!(rendered["agents"]["worker_keep_all"].as_bool(), Some(false));
        assert_eq!(
            defaults.source("agents.worker_retention_days"),
            Some("default")
        );
        assert_eq!(defaults.source("agents.worker_keep_all"), Some("default"));
        for key in [
            "max_depth",
            "max_active_per_parent",
            "max_active_total",
            "max_retained",
        ] {
            assert!(
                rendered["agents"].get(key).is_none(),
                "default cap {key} is set"
            );
        }
        let path = root.path().join("agents.toml");
        std::fs::write(
            &path,
            "[agents]\nmax_depth=3\nmax_active_per_parent=5\nmax_active_total=8\nmax_retained=16\nworker_retention_days=9\nworker_keep_all=true\n",
        )
        .unwrap();
        let mut opts = options(root.path());
        opts.custom_file = Some(path);
        let file = EffectiveConfig::load(opts.clone()).unwrap();
        assert_eq!(file.settings().unwrap().agents.worker_retention_days, 9);
        assert!(file.settings().unwrap().agents.worker_keep_all);
        for key in ["agents.worker_retention_days", "agents.worker_keep_all"] {
            assert!(file.source(key).unwrap().contains("agents.toml"));
        }
        opts.env = vec![
            ("BUBBLEPAW_AGENTS__MAX_DEPTH".into(), "2".into()),
            ("BUBBLEPAW_AGENTS__MAX_ACTIVE_TOTAL".into(), "10".into()),
            ("BUBBLEPAW_AGENTS__WORKER_RETENTION_DAYS".into(), "3".into()),
            ("BUBBLEPAW_AGENTS__WORKER_KEEP_ALL".into(), "false".into()),
        ];
        let env = EffectiveConfig::load(opts.clone()).unwrap();
        assert_eq!(env.settings().unwrap().agents.worker_retention_days, 3);
        assert!(!env.settings().unwrap().agents.worker_keep_all);
        assert_eq!(env.source("agents.worker_retention_days"), Some("env"));
        assert_eq!(env.source("agents.worker_keep_all"), Some("env"));
        opts.cli = vec![
            Override::set("agents.max_depth=0").unwrap(),
            Override::set("agents.worker_retention_days=0").unwrap(),
            Override::set("agents.worker_keep_all=true").unwrap(),
        ];
        let effective = EffectiveConfig::load(opts).unwrap();
        assert_eq!(
            effective.settings().unwrap().agents,
            Agents {
                max_depth: Some(0),
                max_active_per_parent: Some(5),
                max_active_total: Some(10),
                max_retained: Some(16),
                stale_reminder_secs: 7200,
                auto_wake: true,
                wake_debounce_ms: 1500,
                worker_retention_days: 0,
                worker_keep_all: true,
            }
        );
        assert_eq!(effective.source("agents.max_depth"), Some("CLI"));
        assert_eq!(effective.source("agents.max_active_total"), Some("env"));
        assert_eq!(
            effective.source("agents.worker_retention_days"),
            Some("CLI")
        );
        assert_eq!(effective.source("agents.worker_keep_all"), Some("CLI"));
        let rendered: Value = effective.render().unwrap().parse().unwrap();
        assert_eq!(rendered["agents"]["max_depth"].as_integer(), Some(0));
        assert_eq!(
            rendered["agents"]["worker_retention_days"].as_integer(),
            Some(0)
        );
        assert_eq!(rendered["agents"]["worker_keep_all"].as_bool(), Some(true));
        assert_eq!(
            rendered["agents"]["max_active_per_parent"].as_integer(),
            Some(5)
        );
    }

    #[test]
    fn agents_limits_validate_values_and_unknown_keys() {
        let root = tempfile::tempdir().unwrap();
        for entry in [
            "agents.max_active_per_parent=0",
            "agents.max_active_total=0",
            "agents.max_retained=0",
            "agents.max_depth=-1",
            "agents.max_active_total=-1",
            "agents.max_depth=1.5",
            "agents.max_retained='many'",
            "agents.stale_reminder_secs=-1",
            "agents.stale_reminder_secs=1.5",
            "agents.stale_reminder_secs='soon'",
            "agents.worker_retention_days=-1",
            "agents.worker_retention_days=1.5",
            "agents.worker_retention_days='soon'",
            "agents.worker_retention_days=4294967296",
            "agents.worker_keep_all=1",
            "agents.worker_keep_all='yes'",
            "agents.unknown=1",
        ] {
            let mut opts = options(root.path());
            opts.cli = vec![Override::set(entry).unwrap()];
            assert!(EffectiveConfig::load(opts).is_err(), "accepted {entry}");
        }
        let mut opts = options(root.path());
        opts.cli = vec![Override::set("agents.max_retained=1").unwrap()];
        assert_eq!(
            EffectiveConfig::load(opts)
                .unwrap()
                .settings()
                .unwrap()
                .agents
                .max_retained,
            Some(1)
        );
    }

    #[test]
    fn agents_reminder_interval_layers_and_can_be_disabled() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("reminders.toml");
        std::fs::write(&path, "[agents]\nstale_reminder_secs=15\n").unwrap();
        let mut opts = options(root.path());
        opts.custom_file = Some(path);
        opts.env = vec![("BUBBLEPAW_AGENTS__STALE_REMINDER_SECS".into(), "3".into())];
        let effective = EffectiveConfig::load(opts.clone()).unwrap();
        assert_eq!(effective.settings().unwrap().agents.stale_reminder_secs, 3);
        assert_eq!(effective.source("agents.stale_reminder_secs"), Some("env"));
        opts.cli = vec![Override::set("agents.stale_reminder_secs=0").unwrap()];
        let disabled = EffectiveConfig::load(opts).unwrap();
        assert_eq!(disabled.settings().unwrap().agents.stale_reminder_secs, 0);
        assert_eq!(disabled.source("agents.stale_reminder_secs"), Some("CLI"));
    }

    #[test]
    fn web_defaults_and_layering_preserve_sources_without_reading_keys() {
        let root = tempfile::tempdir().unwrap();
        let defaults = EffectiveConfig::load(options(root.path())).unwrap();
        let web = defaults.settings().unwrap().web;
        assert!(!web.enabled);
        assert!(web.expose.fetch && web.expose.search);
        assert_eq!(web.search, vec!["deepseek", "ddg"]);
        assert_eq!(web.deepseek_key_env, "DEEPSEEK_API_KEY");
        assert_eq!(web.deepseek_base_url, crate::net::DEEPSEEK_SEARCH_BASE_URL);
        assert_eq!(web.fetch_backend, vec!["direct"]);
        assert_eq!(web.cooldown_secs, 300);
        assert_eq!(
            (
                web.cache_ttl_secs,
                web.cache_max_bytes,
                web.max_searches,
                web.max_fetches
            ),
            (86400, 64 * 1024 * 1024, None, None)
        );
        assert!(web.net_config().unwrap().is_none());
        let path = root.path().join("web.toml");
        std::fs::write(
            &path,
            r#"
[web]
enabled = true
search = "brave"
key_env = "BUBBLEPAW_SYNTHETIC_BRAVE_SECRET"
max_searches = 3
cache_max_bytes = 1048576
"#,
        )
        .unwrap();
        let mut opts = options(root.path());
        opts.custom_file = Some(path.clone());
        opts.env = vec![
            (
                "BUBBLEPAW_SYNTHETIC_BRAVE_SECRET".into(),
                "never-render-this-value".into(),
            ),
            ("BUBBLEPAW_WEB__MAX_SEARCHES".into(), "4".into()),
            ("BUBBLEPAW_WEB__EXPOSE__SEARCH".into(), "true".into()),
        ];
        opts.cli = vec![Override::set("web.max_searches=5").unwrap()];
        // Loading/rendering enabled Brave succeeds even with no real key in the process.
        let effective = EffectiveConfig::load(opts).unwrap();
        let web = effective.settings().unwrap().web;
        assert!(web.enabled && web.expose.search);
        assert_eq!(web.max_searches, Some(5));
        assert_eq!(web.max_fetches, None);
        assert_eq!(web.cache_max_bytes, 1048576);
        assert_eq!(
            effective.source("web.cache_max_bytes"),
            Some(format!("custom: {}", path.display()).as_str())
        );
        assert_eq!(effective.source("web.max_searches"), Some("CLI"));
        assert_eq!(effective.source("web.expose.search"), Some("env"));
        let rendered = effective.render().unwrap();
        assert!(rendered.contains("BUBBLEPAW_SYNTHETIC_BRAVE_SECRET"));
        assert!(!rendered.contains("never-render-this-value"));
    }

    #[test]
    fn web_config_validates_disabled_settings_and_rejects_credentials() {
        let root = tempfile::tempdir().unwrap();
        for entry in [
            "web.search=invalid",
            "web.search=searxng:file:///tmp/search",
            "web.search=searxng:https://user:secret@example.org",
            "web.fetch_backend=fancy",
            "web.fetch_backend=crawl4ai:",
            "web.fetch_backend=crawl4ai:file:///tmp/crawl",
            "web.fetch_backend=crawl4ai:https://user:secret@example.org",
            "web.search=[]",
            "web.fetch_backend=[]",
            "web.search=[\"ddg\", \"invalid\"]",
            "web.fetch_backend=[\"direct\", \"fancy\"]",
            "web.cooldown_secs=-1",
            "web.max_searches=0",
            "web.max_fetches=0",
            "web.cache_ttl_secs=-1",
            "web.cache_max_bytes=-1",
            "web.research.quick_rounds=6",
            "web.research.model='demo/research'",
            "web.expose.research=true",
            "web.api_key=secret",
            "web.deepseek_api_key=secret",
            "web.deepseek_key_env=''",
            "web.deepseek_base_url=file:///tmp/search",
            "web.deepseek_base_url=https://user:secret@example.org",
            "web.expose.unknown=true",
        ] {
            let mut opts = options(root.path());
            opts.cli = vec![Override::set(entry).unwrap()];
            assert!(EffectiveConfig::load(opts).is_err(), "{entry}");
        }
        let mut opts = options(root.path());
        opts.cli = vec![
            Override::set("web.enabled=true").unwrap(),
            Override::set("web.cache_ttl_secs=0").unwrap(),
        ];
        let settings = EffectiveConfig::load(opts).unwrap().settings().unwrap();
        let config = settings.web.net_config().unwrap().unwrap();
        assert_eq!(config.cache_ttl_secs, 0);
        assert!(matches!(
            config.search.as_slice(),
            [SearchBackend::DeepSeek { base_url, .. }, SearchBackend::DuckDuckGo]
                if base_url == crate::net::DEEPSEEK_SEARCH_BASE_URL
        ));
        assert_eq!(config.fetch, vec![FetchBackend::Direct]);
    }

    #[test]
    fn web_backend_lists_accept_a_string_or_array_in_every_layer() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("web.toml");
        write(
            &path,
            "[web]\nenabled = true\nsearch = [\"searxng:http://127.0.0.1:8888\", \"ddg\"]\n",
        );
        let mut opts = options(root.path());
        opts.custom_file = Some(path);
        opts.env = vec![(
            "BUBBLEPAW_WEB__FETCH_BACKEND".into(),
            "[\"crawl4ai:http://127.0.0.1:11235\", \"direct\"]".into(),
        )];
        opts.cli = vec![Override::set("web.cooldown_secs=60").unwrap()];
        let effective = EffectiveConfig::load(opts).unwrap();
        let settings = effective.settings().unwrap();
        assert_eq!(
            settings.web.search,
            vec!["searxng:http://127.0.0.1:8888", "ddg"]
        );
        assert_eq!(
            settings.web.fetch_backend,
            vec!["crawl4ai:http://127.0.0.1:11235", "direct"]
        );
        let config = settings.web.net_config().unwrap().unwrap();
        assert_eq!(config.search.len(), 2);
        assert_eq!(config.fetch.len(), 2);
        assert_eq!(config.cooldown_secs, 60);
        assert!(
            effective
                .source("web.search")
                .is_some_and(|source| source.starts_with("custom"))
        );
        assert!(effective.render().unwrap().contains("cooldown_secs"));

        // --set accepts the same array syntax, and a bare string stays one entry.
        let mut opts = options(root.path());
        opts.cli = vec![
            Override::set("web.enabled=true").unwrap(),
            Override::set("web.search=[\"brave\", \"ddg\"]").unwrap(),
            Override::set("web.fetch_backend=direct").unwrap(),
        ];
        let settings = EffectiveConfig::load(opts).unwrap().settings().unwrap();
        assert_eq!(settings.web.search, vec!["brave", "ddg"]);
        assert_eq!(settings.web.fetch_backend, vec!["direct"]);
    }

    #[test]
    fn web_deepseek_settings_layer_reload_and_missing_key() {
        let root = tempfile::tempdir().unwrap();
        let initial = EffectiveConfig::load(options(root.path()))
            .unwrap()
            .settings()
            .unwrap();
        let mut opts = options(root.path());
        opts.cli = vec![
            Override::set("web.enabled=true").unwrap(),
            Override::set("web.deepseek_key_env=BUBBLEPAW_TEST_UNSET_DEEPSEEK_KEY").unwrap(),
            Override::set("web.deepseek_base_url=http://127.0.0.1:18081/v1/tools").unwrap(),
        ];
        let candidate = EffectiveConfig::load(opts).unwrap().settings().unwrap();
        let projected = initial.with_live_provider_settings(candidate);
        assert_eq!(
            projected.web.deepseek_key_env,
            "BUBBLEPAW_TEST_UNSET_DEEPSEEK_KEY"
        );
        let config = projected.web.net_config().unwrap().unwrap();
        assert!(matches!(config.search.as_slice(),
            [SearchBackend::DeepSeek { base_url, api_key }, SearchBackend::DuckDuckGo]
                if base_url == "http://127.0.0.1:18081/v1/tools" && api_key.is_empty()));
        assert!(EffectiveConfig::live_paths().contains(&"web.deepseek_key_env"));
        assert!(EffectiveConfig::live_paths().contains(&"web.deepseek_base_url"));
    }

    #[test]
    fn brave_without_a_key_is_not_a_config_error() {
        let root = tempfile::tempdir().unwrap();
        let mut opts = options(root.path());
        opts.cli = vec![
            Override::set("web.enabled=true").unwrap(),
            Override::set("web.search=[\"brave\", \"ddg\"]").unwrap(),
            Override::set("web.key_env=BUBBLEPAW_TEST_UNSET_BRAVE_KEY").unwrap(),
        ];
        let settings = EffectiveConfig::load(opts).unwrap().settings().unwrap();
        let config = settings.web.net_config().unwrap().unwrap();
        assert!(matches!(
            config.search.as_slice(),
            [SearchBackend::Brave { api_key }, SearchBackend::DuckDuckGo] if api_key.is_empty()
        ));
    }

    #[test]
    fn web_fetch_backend_parses_crawl4ai_base_url() {
        let root = tempfile::tempdir().unwrap();
        let mut opts = options(root.path());
        opts.cli = vec![
            Override::set("web.enabled=true").unwrap(),
            Override::set("web.fetch_backend=crawl4ai:http://127.0.0.1:11235").unwrap(),
        ];
        let effective = EffectiveConfig::load(opts).unwrap();
        assert_eq!(effective.source("web.fetch_backend"), Some("CLI"));
        let settings = effective.settings().unwrap();
        let config = settings.web.net_config().unwrap().unwrap();
        assert_eq!(
            config.fetch,
            vec![FetchBackend::Crawl4Ai {
                url: "http://127.0.0.1:11235".into()
            }]
        );
        assert!(effective.render().unwrap().contains("fetch_backend"));
    }

    #[test]
    fn mcp_env_from_names_are_exempt_from_overrides_and_never_render_values() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("mcp.toml");
        write(
            &path,
            "[mcp.servers.github]\ncommand = \"github-mcp-server\"\nargs = [\"stdio\"]\n\
             env = { LOG_LEVEL = \"warn\" }\n\
             env_from = { GITHUB_PERSONAL_ACCESS_TOKEN = \"BUBBLEPAW_SYNTHETIC_GITHUB_TOKEN\" }\n",
        );
        let mut opts = options(root.path());
        opts.custom_file = Some(path);
        // The host variable named by env_from is a credential name, so it is not a config override.
        opts.env = vec![(
            "BUBBLEPAW_SYNTHETIC_GITHUB_TOKEN".into(),
            "never-render-this-value".into(),
        )];
        let effective = EffectiveConfig::load(opts).unwrap();
        let settings = effective.settings().unwrap();
        let github = &settings.mcp.servers["github"];
        assert_eq!(
            github.env_from["GITHUB_PERSONAL_ACCESS_TOKEN"],
            "BUBBLEPAW_SYNTHETIC_GITHUB_TOKEN"
        );
        assert_eq!(github.env["LOG_LEVEL"], "warn");
        let rendered = effective.render().unwrap();
        assert!(rendered.contains("BUBBLEPAW_SYNTHETIC_GITHUB_TOKEN"));
        assert!(!rendered.contains("never-render-this-value"));
    }

    #[test]
    fn mcp_config_rejects_unknown_keys_and_invalid_values() {
        let root = tempfile::tempdir().unwrap();
        for entry in [
            "mcp.servers.github.bogus=true",
            "mcp.servers.github.defer=\"yes\"",
            "mcp.servers.github.approval=\"auto\"",
            "mcp.servers.github.tools.get_issue.approval=\"auto\"",
            "mcp.servers.github.startup_timeout_secs=0",
            "mcp.servers.github.call_timeout_secs=\"soon\"",
            "mcp.servers.github.env_from={ TOKEN = \"A=B\" }",
            "mcp.servers.github.env_from={ TOKEN = \"\" }",
            "mcp.servers.github.env_from={ \"A=B\" = \"TOKEN\" }",
            "mcp.servers.github.env={ \"\" = \"x\" }",
            "mcp.servers.gi.thub.command=\"github\"",
        ] {
            let mut opts = options(root.path());
            opts.cli = vec![
                Override::set("mcp.servers.github.command=\"github-mcp-server\"").unwrap(),
                Override::set(entry).unwrap(),
            ];
            assert!(EffectiveConfig::load(opts).is_err(), "{entry}");
        }
        let mut opts = options(root.path());
        opts.cli = vec![Override::set("mcp.servers.github.command=\"\"").unwrap()];
        assert!(EffectiveConfig::load(opts).is_err());
        let mut opts = options(root.path());
        opts.cli = vec![
            Override::set("mcp.servers.github.command=\"github-mcp-server\"").unwrap(),
            Override::set("mcp.servers.github.approval=\"none\"").unwrap(),
        ];
        assert!(EffectiveConfig::load(opts).is_ok());
    }

    #[test]
    fn mcp_http_layers_allowlist_and_secret_name_exemptions() {
        let root = tempfile::tempdir().unwrap();
        let mut opts = options(root.path());
        opts.env = vec![
            (
                "BUBBLEPAW_MCP__SERVERS__REMOTE__URL".into(),
                "https://example.com/mcp".into(),
            ),
            (
                "BUBBLEPAW_SYNTHETIC_AUTH".into(),
                "Bearer never-render-this-value".into(),
            ),
        ];
        opts.cli = vec![
            Override::set("mcp.header_key_envs=['BUBBLEPAW_SYNTHETIC_AUTH']").unwrap(),
            Override::set(
                "mcp.servers.remote.headers_from={Authorization='BUBBLEPAW_SYNTHETIC_AUTH'}",
            )
            .unwrap(),
        ];
        let effective = EffectiveConfig::load(opts).unwrap();
        let settings = effective.settings().unwrap();
        assert_eq!(
            settings.mcp.header_key_envs,
            vec!["BUBBLEPAW_SYNTHETIC_AUTH"]
        );
        assert_eq!(
            settings.mcp.servers["remote"].url.as_deref(),
            Some("https://example.com/mcp")
        );
        assert_eq!(
            settings.mcp.servers["remote"].headers_from["Authorization"],
            "BUBBLEPAW_SYNTHETIC_AUTH"
        );
        assert_eq!(effective.source("mcp.servers.remote.url"), Some("env"));
        let rendered = effective.render().unwrap();
        assert!(rendered.contains("BUBBLEPAW_SYNTHETIC_AUTH"));
        assert!(!rendered.contains("never-render-this-value"));
        assert!(!format!("{:?}", settings.mcp).contains("never-render-this-value"));
    }

    #[test]
    fn mcp_http_invalid_types_and_values_are_rejected_without_echoing_values() {
        let root = tempfile::tempdir().unwrap();
        for entry in [
            "mcp.header_key_envs='synthetic-secret'",
            "mcp.header_key_envs=['Bearer synthetic-secret']",
            "mcp.servers.remote.url=42",
            "mcp.servers.remote.url='http://example.com/synthetic-secret'",
            "mcp.servers.remote.url='https://user:synthetic-secret@example.com/mcp'",
            "mcp.servers.remote.url='https://example.com/mcp?token=synthetic-secret'",
            "mcp.servers.remote.url='https://example.com/mcp#synthetic-secret'",
            "mcp.servers.remote.command='synthetic-secret'",
            "mcp.servers.remote.args=['synthetic-secret']",
            "mcp.servers.remote.cwd='synthetic-secret'",
            "mcp.servers.remote.env={LOG_LEVEL='synthetic-secret'}",
            "mcp.servers.remote.env_from={TOKEN='SYNTHETIC_TOKEN'}",
            "mcp.servers.remote.headers_from='synthetic-secret'",
            "mcp.servers.remote.headers_from={Authorization=42}",
            "mcp.servers.remote.headers_from={Authorization='Bearer synthetic-secret'}",
            "mcp.servers.remote.headers_from={Authorization='DISALLOWED_AUTH'}",
            "mcp.servers.remote.headers_from={Authorization='MCP_TOKEN',authorization='MCP_TOKEN'}",
            "mcp.servers.remote.headers_from={Accept='MCP_TOKEN'}",
            "mcp.servers.remote.headers_from={'Mcp-Session-Id'='MCP_TOKEN'}",
            "mcp.servers.remote.headers_from={'bad header'='MCP_TOKEN'}",
        ] {
            let mut opts = options(root.path());
            opts.cli = vec![
                Override::set("mcp.servers.remote.url='https://example.com/mcp'").unwrap(),
                Override::set(entry).unwrap(),
            ];
            let error = format!("{:#}", EffectiveConfig::load(opts).expect_err(entry));
            assert!(!error.contains("synthetic-secret"), "{error}");
        }
    }

    #[test]
    fn mcp_http_header_env_override_is_a_reference() {
        let root = tempfile::tempdir().unwrap();
        let mut opts = options(root.path());
        opts.env = vec![
            (
                "BUBBLEPAW_MCP__SERVERS__REMOTE__URL".into(),
                "http://127.0.0.2/mcp".into(),
            ),
            (
                "BUBBLEPAW_MCP__SERVERS__REMOTE__HEADERS_FROM__AUTHORIZATION".into(),
                "BUBBLEPAW_SYNTHETIC_MCP_TOKEN".into(),
            ),
            (
                "BUBBLEPAW_SYNTHETIC_MCP_TOKEN".into(),
                "Bearer never-render-this-value".into(),
            ),
        ];
        let effective = EffectiveConfig::load(opts).unwrap();
        assert_eq!(
            effective.settings().unwrap().mcp.servers["remote"].headers_from["authorization"],
            "BUBBLEPAW_SYNTHETIC_MCP_TOKEN"
        );
        assert!(
            !effective
                .render()
                .unwrap()
                .contains("never-render-this-value")
        );
    }

    #[test]
    fn mcp_http_dotted_header_file_preserves_secret_reference() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("mcp.toml");
        write(
            &path,
            "[mcp.servers.remote]\nurl = 'https://example.com/mcp'\nheaders_from = {'X.Trace' = 'BUBBLEPAW_CUSTOM_TOKEN'}\n",
        );
        let mut opts = options(root.path());
        opts.custom_file = Some(path);
        opts.env = vec![(
            "BUBBLEPAW_CUSTOM_TOKEN".into(),
            "never-render-this-value".into(),
        )];
        let effective = EffectiveConfig::load(opts).unwrap();
        assert_eq!(
            effective.settings().unwrap().mcp.servers["remote"].headers_from["X.Trace"],
            "BUBBLEPAW_CUSTOM_TOKEN"
        );
        let rendered = effective.render().unwrap();
        assert!(rendered.contains("BUBBLEPAW_CUSTOM_TOKEN"));
        assert!(!rendered.contains("never-render-this-value"));
        let reloaded: Value = rendered.parse().unwrap();
        assert_eq!(
            reloaded["mcp"]["servers"]["remote"]["headers_from"]["X.Trace"].as_str(),
            Some("BUBBLEPAW_CUSTOM_TOKEN")
        );
    }

    #[test]
    fn mcp_tool_filters_layer_render_and_preserve_explicit_empty() {
        let root = tempfile::tempdir().unwrap();
        let mut opts = options(root.path());
        opts.cli = vec![
            Override::set("mcp.servers.fixture.command='fixture'").unwrap(),
            Override::set("mcp.servers.fixture.include_tools=['read_*']").unwrap(),
            Override::set("mcp.servers.fixture.exclude_tools=['*_secret']").unwrap(),
        ];
        let effective = EffectiveConfig::load(opts.clone()).unwrap();
        let settings = effective.settings().unwrap();
        assert_eq!(
            settings.mcp.servers["fixture"].include_tools,
            Some(vec!["read_*".into()])
        );
        assert_eq!(settings.mcp.servers["fixture"].exclude_tools, ["*_secret"]);
        assert!(effective.render().unwrap().contains("include_tools"));
        opts.cli
            .push(Override::set("mcp.servers.fixture.include_tools=[]").unwrap());
        let empty = EffectiveConfig::load(opts.clone()).unwrap();
        assert_eq!(
            empty.settings().unwrap().mcp.servers["fixture"].include_tools,
            Some(Vec::new())
        );
        let rendered: Value = empty.render().unwrap().parse().unwrap();
        assert!(
            rendered["mcp"]["servers"]["fixture"]["include_tools"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        for invalid in ["'read_*'", "[1]", "true"] {
            opts.cli.push(
                Override::set(&format!("mcp.servers.fixture.include_tools={invalid}")).unwrap(),
            );
            assert!(EffectiveConfig::load(opts.clone()).is_err());
        }
    }

    fn options(root: &Path) -> LoadOptions {
        LoadOptions {
            workspace: root.join("workspace"),
            home: Some(root.join("home")),
            custom_file: None,
            env: vec![],
            cli: vec![],
        }
    }
    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    fn override_value(path: &str, value: &str) -> Override {
        Override::value(path, parse_value(value))
    }

    #[test]
    fn root_agent_settings_keep_defaults_and_follow_config_layers() {
        let root = tempfile::tempdir().unwrap();
        let mut opts = options(root.path());
        let defaults = EffectiveConfig::load(opts.clone()).unwrap();
        let settings = defaults.settings().unwrap();
        assert_eq!(settings.agent.max_rounds, None);
        assert!(settings.agent.system_prompt.is_none());
        assert_eq!(settings.agent.execution_mode, ExecutionMode::Native);
        write(
            &opts.home.as_ref().unwrap().join(".bubblepaw/config.toml"),
            "[agent]\nsystem_prompt='user instructions'\nmax_rounds=40\n",
        );
        write(
            &opts.workspace.join(".bubblepaw/config.toml"),
            "[agent]\nsystem_prompt='project instructions'\nmax_rounds=48\n",
        );
        let project = EffectiveConfig::load(opts.clone()).unwrap();
        assert_eq!(project.settings().unwrap().agent.max_rounds, Some(48));
        assert_eq!(
            project.settings().unwrap().agent.system_prompt.as_deref(),
            Some("project instructions")
        );
        opts.env = vec![
            (
                "BUBBLEPAW_AGENT__SYSTEM_PROMPT".into(),
                "env instructions".into(),
            ),
            ("BUBBLEPAW_AGENT__MAX_ROUNDS".into(), "64".into()),
        ];
        let environment = EffectiveConfig::load(opts.clone()).unwrap();
        assert_eq!(environment.settings().unwrap().agent.max_rounds, Some(64));
        assert_eq!(
            environment
                .settings()
                .unwrap()
                .agent
                .system_prompt
                .as_deref(),
            Some("env instructions")
        );
        opts.cli = vec![
            override_value("agent.system_prompt", "''"),
            override_value("agent.max_rounds", "1"),
        ];
        let cli = EffectiveConfig::load(opts).unwrap();
        assert_eq!(cli.settings().unwrap().agent.max_rounds, Some(1));
        assert_eq!(
            cli.settings().unwrap().agent.system_prompt.as_deref(),
            Some("")
        );
        assert_eq!(cli.source("agent.max_rounds"), Some("CLI"));
        assert!(cli.restart_needed(&defaults));
        let rendered: Value = cli.render().unwrap().parse().unwrap();
        assert_eq!(rendered["agent"]["system_prompt"].as_str(), Some(""));
    }

    #[test]
    fn root_agent_settings_reject_zero_rounds_and_unknown_keys() {
        let root = tempfile::tempdir().unwrap();
        let mut opts = options(root.path());
        opts.cli = vec![override_value("agent.max_rounds", "0")];
        let error = EffectiveConfig::load(opts).err().unwrap();
        assert!(
            error
                .to_string()
                .contains("agent.max_rounds must be positive")
        );
        let mut opts = options(root.path());
        opts.cli = vec![override_value("agent.round_limit", "4")];
        assert!(
            EffectiveConfig::load(opts)
                .err()
                .unwrap()
                .to_string()
                .contains("Unknown config key: agent.round_limit")
        );
    }

    #[test]
    fn lcm_sizes_are_layered_typed_and_visible_without_round_defaults() {
        let root = tempfile::tempdir().unwrap();
        let mut opts = options(root.path());
        opts.cli = vec![
            override_value("context.lcm.summary_input", "episode"),
            override_value("context.lcm.chapter_rollover_tokens", "0"),
            override_value("context.lcm.source_target_tokens", "16000"),
            override_value("context.lcm.source_max_tokens", "28000"),
            override_value("context.lcm.summary_target_tokens", "1200"),
            override_value("context.lcm.decision_context_tokens", "3000"),
            override_value("context.lcm.tool_evidence_tokens", "6000"),
            override_value("context.lcm.raw_tail_tokens", "16000"),
            override_value("context.lcm.apply_target_tokens", "45000"),
            override_value("context.lcm.output_reserve_tokens", "24000"),
        ];
        let config = EffectiveConfig::load(opts.clone()).unwrap();
        let settings = config.settings().unwrap();
        assert_eq!(settings.agent.max_rounds, None);
        assert_eq!(
            settings.lcm.episodes.summary_input,
            crate::episodes::EpisodeSummaryInput::Episode
        );
        assert_eq!(settings.lcm.episodes.chapter_rollover_tokens, 0);
        assert_eq!(settings.lcm.episodes.source_target_tokens, 16000);
        assert_eq!(settings.lcm.episodes.source_max_tokens, 28000);
        assert_eq!(settings.lcm.episodes.summary_target_tokens, 1200);
        assert_eq!(settings.lcm.episodes.decision_context_tokens, 3000);
        assert_eq!(settings.lcm.episodes.tool_evidence_tokens, 6000);
        assert_eq!(settings.lcm.raw_tail_tokens, 16000);
        assert_eq!(settings.lcm.apply_target_tokens, Some(45000));
        assert_eq!(settings.lcm.output_reserve_tokens, 24000);
        let rendered: Value = config.render().unwrap().parse().unwrap();
        assert_eq!(
            rendered["context"]["lcm"]["summary_input"].as_str(),
            Some("episode")
        );
        assert_eq!(
            rendered["context"]["lcm"]["source_target_tokens"].as_integer(),
            Some(16000)
        );
        assert!(rendered["agent"].get("max_rounds").is_none());
        opts.cli
            .push(override_value("context.lcm.source_max_tokens", "1000"));
        assert!(EffectiveConfig::load(opts).is_err());
    }

    #[test]
    fn approval_deny_layers_validation_and_reload_classification() {
        let root = tempfile::tempdir().unwrap();
        let mut opts = options(root.path());
        let file = opts.workspace.join(".bubblepaw/config.toml");
        write(&file, "[approvals]\ndeny=['exec','mcp__server__erase']\n");
        let base = EffectiveConfig::load(opts.clone()).unwrap();
        assert_eq!(
            base.settings().unwrap().approvals.deny,
            ["exec", "mcp__server__erase"]
        );
        assert!(
            base.source("approvals.deny")
                .unwrap()
                .starts_with("project:")
        );
        opts.cli = vec![Override::set("approvals.deny=['edit']").unwrap()];
        let next = EffectiveConfig::load(opts.clone()).unwrap();
        assert_eq!(next.settings().unwrap().approvals.deny, ["edit"]);
        assert!(!base.same_provider_config(&next));
        assert!(!next.restart_needed(&base));
        for text in [
            "[approvals]\ndeny=['']\n",
            "[approvals]\ndeny=['   ']\n",
            "[approvals]\ndeny='exec'\n",
            "[approvals]\ndeny=[123]\n",
            "[approval]\ndeny=['exec']\n",
        ] {
            write(&file, text);
            let mut invalid = opts.clone();
            invalid.cli.clear();
            assert!(EffectiveConfig::load(invalid).is_err(), "{text}");
        }
        write(&file, "[approvals]\ndeny=['exec']\n");
        opts.cli.clear();
        assert_eq!(
            EffectiveConfig::load(opts)
                .unwrap()
                .settings()
                .unwrap()
                .approvals
                .deny,
            ["exec"]
        );
    }

    #[test]
    fn hardline_floor_defaults_on_and_layers_like_other_approval_settings() {
        let root = tempfile::tempdir().unwrap();
        let mut opts = options(root.path());
        let file = opts.workspace.join(".bubblepaw/config.toml");
        let defaults = EffectiveConfig::load(opts.clone()).unwrap();
        assert!(defaults.settings().unwrap().approvals.hardline);
        assert!(
            crate::live_config::approval_policy(&defaults.settings().unwrap())
                .unwrap()
                .hardline
        );
        write(&file, "[approvals]\nhardline=false\n");
        let off = EffectiveConfig::load(opts.clone()).unwrap();
        assert!(!off.settings().unwrap().approvals.hardline);
        assert!(
            !crate::live_config::approval_policy(&off.settings().unwrap())
                .unwrap()
                .hardline
        );
        assert!(
            off.source("approvals.hardline")
                .unwrap()
                .starts_with("project:")
        );
        opts.cli = vec![Override::set("approvals.hardline=true").unwrap()];
        assert!(
            EffectiveConfig::load(opts.clone())
                .unwrap()
                .settings()
                .unwrap()
                .approvals
                .hardline
        );
        for text in ["[approvals]\nhardline='no'\n", "[approvals]\nhardline=1\n"] {
            write(&file, text);
            let mut invalid = opts.clone();
            invalid.cli.clear();
            assert!(EffectiveConfig::load(invalid).is_err(), "{text}");
        }
    }

    #[test]
    fn layer_precedence_default_user_project_env_cli() {
        let root = tempfile::tempdir().unwrap();
        let mut opts = options(root.path());
        assert_eq!(
            EffectiveConfig::load(options(root.path()))
                .unwrap()
                .settings()
                .unwrap()
                .compact_threshold_tokens,
            200_000
        );
        write(
            &opts.home.as_ref().unwrap().join(".bubblepaw/config.toml"),
            "[context]\ncompact_threshold_tokens=100\n",
        );
        assert_eq!(
            EffectiveConfig::load(options(root.path()))
                .unwrap()
                .settings()
                .unwrap()
                .compact_threshold_tokens,
            100
        );
        write(
            &opts.workspace.join(".bubblepaw/config.toml"),
            "[context]\ncompact_threshold_tokens=200\n",
        );
        assert_eq!(
            EffectiveConfig::load(options(root.path()))
                .unwrap()
                .settings()
                .unwrap()
                .compact_threshold_tokens,
            200
        );
        opts.env.push((
            "BUBBLEPAW_CONTEXT__COMPACT_THRESHOLD_TOKENS".into(),
            "300".into(),
        ));
        assert_eq!(
            EffectiveConfig::load(LoadOptions {
                cli: vec![],
                ..options_with_env(root.path(), opts.env.clone())
            })
            .unwrap()
            .settings()
            .unwrap()
            .compact_threshold_tokens,
            300
        );
        opts.cli
            .push(override_value("context.compact_threshold_tokens", "400"));
        let config = EffectiveConfig::load(opts).unwrap();
        assert_eq!(config.settings().unwrap().compact_threshold_tokens, 400);
        assert_eq!(
            config.source("context.compact_threshold_tokens"),
            Some("CLI")
        );
    }
    fn options_with_env(root: &Path, env: Vec<(String, String)>) -> LoadOptions {
        LoadOptions {
            env,
            ..options(root)
        }
    }

    #[test]
    fn explicit_config_replaces_user_and_project_and_requires_file() {
        let root = tempfile::tempdir().unwrap();
        let mut opts = options(root.path());
        write(
            &opts.home.as_ref().unwrap().join(".bubblepaw/config.toml"),
            "[model]\nmain='user'\n",
        );
        write(
            &opts.workspace.join(".bubblepaw/config.toml"),
            "[context]\nstrategy='classical'\n",
        );
        let custom = root.path().join("custom.toml");
        write(&custom, "[model]\nmain='custom'\n");
        opts.custom_file = Some(custom.clone());
        let config = EffectiveConfig::load(opts).unwrap();
        assert_eq!(config.settings().unwrap().main_model, "custom");
        assert!(matches!(
            config.settings().unwrap().context_strategy,
            ContextStrategy::Lcm
        ));
        let config = EffectiveConfig::load(options_with_env(
            root.path(),
            vec![(
                "BUBBLEPAW_CONFIG".into(),
                custom.to_string_lossy().into_owned(),
            )],
        ))
        .unwrap();
        assert_eq!(config.settings().unwrap().main_model, "custom");
        let mut opts = options(root.path());
        opts.custom_file = Some(root.path().join("missing.toml"));
        assert!(EffectiveConfig::load(opts).is_err());
        let chosen = root.path().join("chosen.toml");
        write(&chosen, "[model]\nmain='chosen'\n");
        let mut opts = options_with_env(
            root.path(),
            vec![(
                "BUBBLEPAW_CONFIG".into(),
                custom.to_string_lossy().into_owned(),
            )],
        );
        opts.custom_file = Some(chosen);
        assert_eq!(
            EffectiveConfig::load(opts)
                .unwrap()
                .settings()
                .unwrap()
                .main_model,
            "chosen"
        );
    }

    #[test]
    fn env_double_underscore_maps_nested_paths() {
        let root = tempfile::tempdir().unwrap();
        let config = EffectiveConfig::load(options_with_env(
            root.path(),
            vec![
                ("BUBBLEPAW_CONTEXT__STRATEGY".into(), "classical".into()),
                (
                    "BUBBLEPAW_CONTEXT__CLASSICAL__VERBATIM_USER_TOKENS".into(),
                    "1000".into(),
                ),
                (
                    "BUBBLEPAW_PROVIDERS__OPENCODE__REASONING__REASONING_EFFORT".into(),
                    "medium".into(),
                ),
                ("BUBBLEPAW_APPROVALS__TOOLS__BASH".into(), "allow".into()),
            ],
        ))
        .unwrap();
        let settings = config.settings().unwrap();
        assert!(matches!(
            settings.context_strategy,
            ContextStrategy::Classical
        ));
        assert_eq!(settings.verbatim_user_tokens, 1000);
        assert_eq!(settings.approvals.tools["bash"], "allow");
        assert_eq!(
            settings.endpoints.opencode_reasoning_fields["reasoning_effort"],
            "medium"
        );
    }

    #[test]
    fn set_parses_toml_values_and_falls_back_to_strings() {
        assert_eq!(
            Override::set("model.main=provider/my-model").unwrap().value,
            Value::String("provider/my-model".into())
        );
        assert_eq!(
            Override::set("context.compact_threshold_tokens=1234")
                .unwrap()
                .value,
            Value::Integer(1234)
        );
        assert_eq!(
            Override::set("context.auto_summarize=false").unwrap().value,
            Value::Boolean(false)
        );
        assert!(
            Override::set("roles.summary=['main']")
                .unwrap()
                .value
                .is_array()
        );
        assert!(
            Override::set("roles.summary={models=['main'], repeats=2}")
                .unwrap()
                .value
                .is_table()
        );
        assert_eq!(
            parse_value("'first'\nsecond='value'"),
            Value::String("'first'\nsecond='value'".into())
        );
        assert!(Override::set("missing-equals").is_err());
    }

    #[test]
    fn unknown_file_key_names_exact_path() {
        let root = tempfile::tempdir().unwrap();
        let opts = options(root.path());
        write(
            &opts.workspace.join(".bubblepaw/config.toml"),
            "[context]\nmisspelled=1\n",
        );
        assert!(
            EffectiveConfig::load(opts)
                .unwrap_err()
                .to_string()
                .contains("context.misspelled")
        );
    }
    #[test]
    fn unknown_env_key_names_exact_path() {
        let root = tempfile::tempdir().unwrap();
        let error = EffectiveConfig::load(options_with_env(
            root.path(),
            vec![("BUBBLEPAW_CONTEXT__MISSPELLED".into(), "1".into())],
        ))
        .unwrap_err();
        assert!(error.to_string().contains("context.misspelled"));
    }
    #[test]
    fn unknown_set_key_names_exact_path() {
        let root = tempfile::tempdir().unwrap();
        let mut opts = options(root.path());
        opts.cli
            .push(Override::set("context.misspelled=1").unwrap());
        assert!(
            EffectiveConfig::load(opts)
                .unwrap_err()
                .to_string()
                .contains("context.misspelled")
        );
    }

    #[test]
    fn role_references_main_summary_and_llm_summary_resolve() {
        let root = tempfile::tempdir().unwrap();
        let mut opts = options(root.path());
        opts.cli = vec![
            Override::set("roles.summary=['opencode/fixture','main']").unwrap(),
            Override::set("roles.chapter='summary'").unwrap(),
            Override::set("roles.decider='llm/summary'").unwrap(),
        ];
        let roles = EffectiveConfig::load(opts)
            .unwrap()
            .settings()
            .unwrap()
            .roles;
        assert_eq!(
            roles.summary.models,
            ["opencode/fixture", "codex/gpt-6-luna"]
        );
        assert_eq!(roles.chapter.models, roles.summary.models);
        assert_eq!(
            roles.decider.models,
            ["llm/opencode/fixture", "llm/codex/gpt-6-luna"]
        );
        assert_eq!(roles.summary.repeats, Some(1));
        assert_eq!(roles.summary.threshold, Some(0.0));
    }

    #[test]
    fn cycles_and_bad_role_settings_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        for entries in [
            vec!["roles.summary='chapter'", "roles.chapter='summary'"],
            vec!["roles.decider.repeats=0"],
            vec!["roles.approver.threshold=1.1"],
            vec!["roles.summary=[]"],
            vec!["roles.summary='missing'"],
        ] {
            let mut opts = options(root.path());
            opts.cli = entries
                .into_iter()
                .map(|entry| Override::set(entry).unwrap())
                .collect();
            assert!(EffectiveConfig::load(opts).is_err());
        }
    }

    #[test]
    fn role_reasoning_effort_layers_validate_and_keep_defaults() {
        let root = tempfile::tempdir().unwrap();
        let defaults = EffectiveConfig::load(options(root.path()))
            .unwrap()
            .settings()
            .unwrap();
        for role in [
            &defaults.roles.summary,
            &defaults.roles.chapter,
            &defaults.roles.decider,
            &defaults.roles.approver,
        ] {
            assert_eq!(role.reasoning_effort, ReasoningEffort::Low);
        }
        let custom = root.path().join("effort.toml");
        std::fs::write(&custom, "[roles.summary]\nreasoning_effort='medium'\n").unwrap();
        let mut opts = options(root.path());
        opts.custom_file = Some(custom.clone());
        let file = EffectiveConfig::load(opts).unwrap();
        assert_eq!(
            file.settings().unwrap().roles.summary.reasoning_effort,
            ReasoningEffort::Medium
        );
        assert_eq!(
            file.source("roles.summary.reasoning_effort"),
            Some(format!("custom: {}", custom.display()).as_str())
        );
        let mut opts = options(root.path());
        opts.custom_file = Some(custom.clone());
        opts.env = vec![(
            "BUBBLEPAW_ROLES__SUMMARY__REASONING_EFFORT".into(),
            "high".into(),
        )];
        let env = EffectiveConfig::load(opts).unwrap();
        assert_eq!(
            env.settings().unwrap().roles.summary.reasoning_effort,
            ReasoningEffort::High
        );
        let mut opts = options(root.path());
        opts.custom_file = Some(custom);
        opts.env = vec![(
            "BUBBLEPAW_ROLES__SUMMARY__REASONING_EFFORT".into(),
            "high".into(),
        )];
        opts.cli = vec![
            Override::set("roles.summary.reasoning_effort=max").unwrap(),
            Override::set("roles.summary=[\"opencode-messages/claude-haiku-5-5\"]").unwrap(),
        ];
        let cli = EffectiveConfig::load(opts).unwrap();
        assert_eq!(
            cli.settings().unwrap().roles.summary.reasoning_effort,
            ReasoningEffort::Max
        );
        assert_eq!(
            cli.settings().unwrap().roles.chapter.reasoning_effort,
            ReasoningEffort::Low
        );
        let rendered: Value = cli.render().unwrap().parse().unwrap();
        assert_eq!(
            rendered["roles"]["summary"]["reasoning_effort"].as_str(),
            Some("max")
        );
        for value in ["low", "medium", "high", "xhigh", "max"] {
            let mut opts = options(root.path());
            opts.cli =
                vec![Override::set(&format!("roles.decider.reasoning_effort={value}")).unwrap()];
            assert!(EffectiveConfig::load(opts).is_ok());
        }
        for entry in [
            "roles.summary.reasoning_effort=none",
            "roles.decider.reasoning_effort=disabled",
            "roles.chapter.reasoning_effort=minimal",
            "providers.codex.reasoning.reasoning_effort=none",
            "providers.opencode.reasoning.thinking={type='disabled'}",
        ] {
            let mut opts = options(root.path());
            opts.cli = vec![Override::set(entry).unwrap()];
            assert!(EffectiveConfig::load(opts).is_err(), "{entry}");
        }
    }

    #[test]
    fn messages_provider_config_resolves_roles_and_custom_endpoints() {
        let root = tempfile::tempdir().unwrap();
        let mut opts = options(root.path());
        opts.cli = vec![
            Override::set("roles.summary=[\"opencode-messages/claude-haiku-5-5\"]").unwrap(),
            Override::set("providers.opencode-messages.base_url='http://127.0.0.1:12345/v1'").unwrap(),
            Override::set("providers.opencode-messages.key_env='MESSAGES_FIXTURE_KEY'").unwrap(),
            Override::set("providers.anthropic={kind='anthropic-messages',base_url='https://api.anthropic.com/v1',key_env='ANTHROPIC_API_KEY'}").unwrap(),
        ];
        let config = EffectiveConfig::load(opts).unwrap();
        let settings = config.settings().unwrap();
        assert_eq!(
            settings.roles.summary.models,
            ["opencode-messages/claude-haiku-5-5"]
        );
        assert_eq!(settings.roles.chapter.models, settings.roles.summary.models);
        assert_eq!(
            settings.providers["anthropic"].kind.as_deref(),
            Some("anthropic-messages")
        );
        assert_eq!(
            settings.endpoints.opencode_messages_base_url.as_deref(),
            Some("http://127.0.0.1:12345/v1")
        );
        assert_eq!(
            settings.endpoints.opencode_messages_key_env,
            "MESSAGES_FIXTURE_KEY"
        );
        assert!(config.render().unwrap().contains("anthropic-messages"));

        let mut opts = options(root.path());
        opts.cli = vec![
            Override::set("providers.opencode-messages.reasoning={thinking={type='disabled'}}")
                .unwrap(),
        ];
        assert!(EffectiveConfig::load(opts).is_err());
    }

    #[test]
    fn no_config_defaults_equal_pinned_model_roles_and_endpoints() {
        let root = tempfile::tempdir().unwrap();
        let settings = EffectiveConfig::load(options(root.path()))
            .unwrap()
            .settings()
            .unwrap();
        assert_eq!(settings.main_model, "gpt-6-luna");
        assert_eq!(settings.main_provider, "codex");
        let expected = ModelRoles::for_models(
            "opencode/deepseek-v4.1-flash".into(),
            "codex/gpt-6-luna".into(),
        );
        for (actual, expected) in [
            (&settings.roles.summary, &expected.summary),
            (&settings.roles.chapter, &expected.chapter),
            (&settings.roles.decider, &expected.decider),
            (&settings.roles.approver, &expected.approver),
        ] {
            assert_eq!(actual.models, expected.models);
            assert_eq!(actual.repeats, expected.repeats);
            assert_eq!(actual.threshold, expected.threshold);
        }
        let expected = ProviderEndpoints::default();
        assert_eq!(
            settings.endpoints.opencode_base_url,
            expected.opencode_base_url
        );
        assert_eq!(
            settings.endpoints.opencode_key_env,
            expected.opencode_key_env
        );
        assert_eq!(
            settings.endpoints.opencode_reasoning_fields,
            expected.opencode_reasoning_fields
        );
        assert_eq!(
            settings.endpoints.opencode_messages_base_url,
            expected.opencode_messages_base_url
        );
        assert_eq!(
            settings.endpoints.opencode_messages_key_env,
            expected.opencode_messages_key_env
        );
        assert_eq!(settings.endpoints.openai_base_url, expected.openai_base_url);
        assert_eq!(settings.endpoints.openai_key_env, expected.openai_key_env);
        assert_eq!(
            settings.endpoints.openrouter_decisions_url,
            expected.openrouter_decisions_url
        );
        assert_eq!(
            settings.endpoints.openrouter_key_env,
            expected.openrouter_key_env
        );
        assert_eq!(settings.compact_threshold_tokens, 200_000);
        assert_eq!(settings.verbatim_user_tokens, 20_000);
        assert_eq!(settings.retry_base_ms, 500);
        assert_eq!(settings.approvals.confirm, "ask");
    }

    #[test]
    fn config_output_shows_sources_and_never_provider_secret_values() {
        let root = tempfile::tempdir().unwrap();
        let mut opts = options_with_env(
            root.path(),
            vec![
                (
                    "BUBBLEPAW_PRIVATE_KEY".into(),
                    "secret-never-print-this".into(),
                ),
                ("OPENAI_API_KEY".into(), "second-secret".into()),
            ],
        );
        opts.cli = vec![
            Override::set("providers.opencode.key_env='BUBBLEPAW_PRIVATE_KEY'").unwrap(),
            Override::set("context.strategy=classical").unwrap(),
        ];
        let config = EffectiveConfig::load(opts).unwrap();
        let rendered = config.render().unwrap();
        assert!(rendered.contains("# source: default"));
        assert!(rendered.contains("# source: CLI"));
        assert!(rendered.contains("BUBBLEPAW_PRIVATE_KEY"));
        assert!(!rendered.contains("secret-never-print-this"));
        assert!(!rendered.contains("second-secret"));
        let parsed: Value = rendered.parse().unwrap();
        assert_eq!(parsed, config.value);
    }

    #[test]
    fn extensions_table_equals_old_json_and_resolves_relative_cwd() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("extensions.json");
        write(
            &path,
            r#"[{"id":"fixture","command":"worker","args":["--test"],"cwd":"relative"}]"#,
        );
        let old = load_extensions(&path).unwrap();
        let mut opts = options(root.path());
        opts.cli.push(Override::extensions_json(&path).unwrap());
        let settings = EffectiveConfig::load(opts).unwrap().settings().unwrap();
        assert_eq!(
            serde_json::to_value(&settings.extensions).unwrap(),
            serde_json::to_value(&old).unwrap()
        );
        let custom = root.path().join("config.toml");
        write(
            &custom,
            "[extensions.fixture]\ncommand='worker'\nargs=['--test']\ncwd='relative'\n",
        );
        let mut opts = options(root.path());
        opts.custom_file = Some(custom);
        let settings = EffectiveConfig::load(opts).unwrap().settings().unwrap();
        assert_eq!(
            serde_json::to_value(&settings.extensions).unwrap(),
            serde_json::to_value(&old).unwrap()
        );
    }

    #[test]
    fn quota_defaults_and_provider_accounts_use_entry_names() {
        let root = tempfile::tempdir().unwrap();
        let config = EffectiveConfig::load(options(root.path())).unwrap();
        let settings = config.settings().unwrap();
        assert_eq!(settings.quota, QuotaSettings::default());
        assert_eq!(settings.quota.max_wait_seconds, 21600);
        assert_eq!(config.source("quota.max_wait_seconds"), Some("default"));
        for (route, account) in settings.provider_accounts() {
            assert_eq!(route, account);
        }
    }

    #[test]
    fn provider_and_extension_aliases_can_share_an_account() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        write(
            &path,
            "[providers.codex]\naccount='personal'\n\
             [providers.opencode]\naccount='personal'\n\
             [extensions.claude]\ncommand='bun'\naccount='personal'\n\
             [extensions.other]\ncommand='worker'\n\
             [quota]\nmax_wait_seconds=0\n",
        );
        let mut opts = options(root.path());
        opts.custom_file = Some(path);
        let config = EffectiveConfig::load(opts).unwrap();
        let settings = config.settings().unwrap();
        let accounts = settings.provider_accounts();
        assert_eq!(accounts["codex"], "personal");
        assert_eq!(accounts["opencode"], "personal");
        assert_eq!(accounts["claude"], "personal");
        assert_eq!(accounts["other"], "other");
        assert_eq!(settings.quota.max_wait_seconds, 0);
        let rendered: Value = config.render().unwrap().parse().unwrap();
        assert_eq!(
            rendered["extensions"]["claude"]["account"].as_str(),
            Some("personal")
        );
    }

    #[test]
    fn extension_json_preserves_account_override() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("extensions.json");
        write(
            &path,
            r#"[{"id":"claude","command":"bun","account":"shared"}]"#,
        );
        let mut opts = options(root.path());
        opts.cli.push(Override::extensions_json(&path).unwrap());
        let settings = EffectiveConfig::load(opts).unwrap().settings().unwrap();
        assert_eq!(settings.provider_accounts()["claude"], "shared");
        assert_eq!(settings.extensions[0].account.as_deref(), Some("shared"));
    }

    #[test]
    fn account_labels_and_quota_wait_are_validated_without_credentials() {
        let root = tempfile::tempdir().unwrap();
        for value in ["''", "' '", "' padded '", "\"bad\\nlabel\""] {
            for route in ["providers.codex", "extensions.fixture"] {
                let mut opts = options(root.path());
                if route.starts_with("extensions") {
                    opts.cli
                        .push(Override::set("extensions.fixture.command=worker").unwrap());
                }
                opts.cli
                    .push(Override::set(&format!("{route}.account={value}")).unwrap());
                assert!(EffectiveConfig::load(opts).is_err(), "{route}: {value}");
            }
        }
        let mut opts = options(root.path());
        opts.cli
            .push(Override::set("quota.max_wait_seconds=-1").unwrap());
        assert!(EffectiveConfig::load(opts).is_err());
        let mut opts = options(root.path());
        opts.cli.push(Override::value(
            "providers.codex.account",
            Value::String("a".repeat(257)),
        ));
        assert!(EffectiveConfig::load(opts).is_err());
    }

    #[test]
    fn quota_wait_changes_need_restart_while_provider_accounts_are_live() {
        let root = tempfile::tempdir().unwrap();
        let startup = EffectiveConfig::load(options(root.path())).unwrap();
        let mut opts = options(root.path());
        opts.cli
            .push(Override::set("providers.codex.account=shared").unwrap());
        let live = EffectiveConfig::load(opts.clone()).unwrap();
        assert!(!live.restart_needed(&startup));
        opts.cli
            .push(Override::set("quota.max_wait_seconds=7").unwrap());
        let restart = EffectiveConfig::load(opts).unwrap();
        assert!(restart.restart_needed(&startup));
    }

    #[test]
    fn extension_env_table_reaches_settings_and_render_redacts_credentials() {
        let root = tempfile::tempdir().unwrap();
        let custom = root.path().join("config.toml");
        write(
            &custom,
            "[extensions.claude-code-b]\ncommand='bun'\nargs=['run','src/main.ts']\n\
             [extensions.claude-code-b.env]\nCLAUDE_CONFIG_DIR='/home/user/.claude-b'\n\
             BUBBLEPAW_CC_SYSTEM_PROMPT='replace'\nANTHROPIC_API_KEY='sk-private'\n",
        );
        let mut opts = options(root.path());
        opts.custom_file = Some(custom);
        let config = EffectiveConfig::load(opts).unwrap();
        let settings = config.settings().unwrap();
        let env = &settings.extensions[0].env;
        assert_eq!(env["CLAUDE_CONFIG_DIR"], "/home/user/.claude-b");
        assert_eq!(env["BUBBLEPAW_CC_SYSTEM_PROMPT"], "replace");
        let rendered = config.render().unwrap();
        assert!(rendered.contains("/home/user/.claude-b"), "{rendered}");
        assert!(!rendered.contains("sk-private"), "{rendered}");
    }

    #[test]
    fn partial_role_overrides_preserve_models_after_shorthand() {
        let root = tempfile::tempdir().unwrap();
        let mut opts = options(root.path());
        write(
            &opts.workspace.join(".bubblepaw/config.toml"),
            "[roles]\nsummary='main'\n",
        );
        opts.cli
            .push(Override::set("roles.summary.repeats=2").unwrap());
        let role = EffectiveConfig::load(opts)
            .unwrap()
            .settings()
            .unwrap()
            .roles
            .summary;
        assert_eq!(role.models, ["codex/gpt-6-luna"]);
        assert_eq!(role.repeats, Some(2));
    }

    #[test]
    fn empty_implicit_file_keeps_defaults() {
        let root = tempfile::tempdir().unwrap();
        let opts = options(root.path());
        write(
            &opts.workspace.join(".bubblepaw/config.toml"),
            "# no overrides\n",
        );
        assert_eq!(
            EffectiveConfig::load(opts)
                .unwrap()
                .settings()
                .unwrap()
                .main_model,
            "gpt-6-luna"
        );
    }

    #[test]
    fn quoted_env_key_name_exempts_prefixed_credentials() {
        let root = tempfile::tempdir().unwrap();
        let config = EffectiveConfig::load(options_with_env(
            root.path(),
            vec![
                (
                    "BUBBLEPAW_PROVIDERS__OPENCODE__KEY_ENV".into(),
                    "'BUBBLEPAW_PRIVATE_KEY'".into(),
                ),
                ("BUBBLEPAW_PRIVATE_KEY".into(), "secret-value".into()),
            ],
        ))
        .unwrap();
        assert_eq!(
            config.settings().unwrap().endpoints.opencode_key_env,
            "BUBBLEPAW_PRIVATE_KEY"
        );
        assert!(!config.render().unwrap().contains("secret-value"));
    }

    #[test]
    fn malformed_set_path_and_provider_id_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        for assignment in [
            "context..strategy=lcm",
            "providers.invalid/name={kind='responses',base_url='http://localhost',key_env='KEY'}",
        ] {
            let mut opts = options(root.path());
            opts.cli.push(Override::set(assignment).unwrap());
            assert!(EffectiveConfig::load(opts).is_err());
        }
    }

    #[test]
    fn disabled_default_summary_preserves_explicit_decision_models() {
        let root = tempfile::tempdir().unwrap();
        let mut opts = options(root.path());
        opts.cli = vec![
            Override::set("providers.opencode.base_url=''").unwrap(),
            Override::set("roles.decider='llm/opencode/deepseek-v4.1-flash'").unwrap(),
            Override::set("roles.chapter='summary'").unwrap(),
        ];
        let settings = EffectiveConfig::load(opts).unwrap().settings().unwrap();
        assert_eq!(
            settings.roles.decider.models,
            ["llm/opencode/deepseek-v4.1-flash"]
        );
        assert_eq!(settings.roles.chapter.models, ["codex/gpt-6-luna"]);
    }

    #[test]
    fn configured_scripted_alias_registers_without_enabling_default_demo() {
        let root = tempfile::tempdir().unwrap();
        let mut opts = options(root.path());
        opts.cli
            .push(Override::set("providers.fixture={kind='scripted'}").unwrap());
        let settings = EffectiveConfig::load(opts).unwrap().settings().unwrap();
        let mut registry = ProviderRegistry::new(
            "main",
            Arc::new(crate::provider::ScriptedProvider::new("fixture")),
        );
        settings.register_providers(&mut registry).unwrap();
        assert!(registry.get("fixture").is_some());
        assert!(registry.get("demo").is_none());
    }

    #[test]
    fn quoted_dotted_file_key_is_not_a_nested_known_key() {
        let root = tempfile::tempdir().unwrap();
        let opts = options(root.path());
        write(
            &opts.workspace.join(".bubblepaw/config.toml"),
            "'context.strategy'='classical'\n",
        );
        assert!(
            EffectiveConfig::load(opts)
                .unwrap_err()
                .to_string()
                .contains("Unknown config key: context.strategy")
        );
    }
}

#[cfg(test)]
#[path = "config_exec_tests.rs"]
mod exec_tests;

#[cfg(test)]
#[path = "config_execution_mode_tests.rs"]
mod execution_mode_tests;

#[cfg(test)]
#[path = "config_profile_tests.rs"]
mod profile_tests;
