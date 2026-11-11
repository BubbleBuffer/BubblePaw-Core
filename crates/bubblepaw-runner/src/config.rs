//! Runner-owned layered settings. Credentials are named, never read by the loader.
use crate::{
    model_roles::ModelRoles,
    net::{FetchBackend, SearchBackend, WebConfig, one_or_many},
    provider::{
        ChatCompletionsProvider, ExtensionConfig, MessagesProvider, OpenAiDecisions,
        OpenCodeDecisions, OpenRouterDecisions, ProviderEndpoints, ProviderRegistry,
        ResponsesProvider, load_extensions,
    },
    toolsets::Toolset,
};
use anyhow::{Context, Result, bail, ensure};
use bubblepaw_core::{ContextStrategy, ExecutionMode, ReasoningEffort, ReasoningReplay};
use bubblepaw_core::{
    config_schema::{LIVE, Node, RESTART},
    config_section,
};
use bubblepaw_mcp::McpSettings;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};
use toml::Value;

#[path = "config/file_layers.rs"]
mod file_layers;
#[path = "config/live_settings.rs"]
mod live_settings;
#[path = "config/mcp_validate.rs"]
mod mcp_validate;
pub use mcp_validate::LeafError;
use mcp_validate::validate_mcp;
#[path = "config/key_names.rs"]
mod key_names;
#[path = "config/redact.rs"]
pub(crate) mod redact;
#[path = "config/roles.rs"]
mod roles;
#[path = "config/schema.rs"]
mod schema;
#[path = "config/sections.rs"]
mod sections;
#[path = "config/skills_section.rs"]
mod skills_section;
use key_names::{collect_key_names, key_names};
#[path = "config/environment.rs"]
mod environment;
#[path = "config/loading.rs"]
mod loading;
#[path = "config/provider_checks.rs"]
mod provider_checks;
#[path = "config/provider_register.rs"]
mod provider_register;
#[path = "config/remote_section.rs"]
mod remote_section;
#[path = "config/resolve.rs"]
mod resolve;
#[path = "config/via.rs"]
mod via;
use provider_checks::{RoleEntry, endpoints, valid_id, validate_account};
pub use provider_checks::{selected_main_provider, validate_provider_setup};
#[path = "config/value_paths.rs"]
mod value_paths;
pub use redact::SettingEntry;
pub use remote_section::{RemoteMode, RemoteSettings};
use roles::resolve_role;
pub use schema::Change;
use schema::{REASONING, known_path};
use sections::ExtensionSettings;
pub use sections::{
    Agents, ApprovalSettings, DelegateSettings, ExecConfig, ModelClass, ProviderModelSettings,
    ProviderSettings, QuotaSettings, RootAgentSettings, StorageSettings, WebExpose, WebSettings,
};
pub use skills_section::{SkillToggle, SkillsConfig};
use value_paths::{
    collect_sources, join_path, merge_at, normalize_roles, record_override, render_leaves,
    validate_keys,
};
pub use value_paths::{is_opencode_route, parse_value};

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
    pub toolsets: BTreeMap<String, Toolset>,
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
    pub remote: RemoteSettings,
    pub extensions: Vec<ExtensionConfig>,
    /// Live host servers; secrets are resolved at connection time only.
    pub mcp: McpSettings,
    pub delegate: DelegateSettings,
    configured_providers: BTreeSet<String>,
}
#[cfg(test)]
#[path = "config_mcp_tests.rs"]
mod mcp_tests;

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "config_exec_tests.rs"]
mod exec_tests;

#[cfg(test)]
#[path = "config_execution_mode_tests.rs"]
mod execution_mode_tests;

#[cfg(test)]
#[path = "config_profile_tests.rs"]
mod profile_tests;

#[cfg(test)]
#[path = "config_schema_tests.rs"]
mod schema_tests;
