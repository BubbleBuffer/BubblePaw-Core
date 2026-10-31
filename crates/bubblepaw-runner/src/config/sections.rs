//! Settings sections: one struct per config section, with its key schema, defaults and validation.
use super::*;

config_section! {
    /// Skill directories add collections after project and user skills.
    #[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
    #[serde(default)]
    pub struct SkillsConfig {
        pub enabled: bool => LIVE,
        pub dirs: Vec<PathBuf> => LIVE,
    }
}

config_section! {
    /// Maximum automatic wait for a provider account's quota reset.
    #[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
    #[serde(default)]
    pub struct QuotaSettings {
        /// Zero declines automatic waiting. It does not disable quota checks.
        pub max_wait_seconds: u64 => RESTART,
    }
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

config_section! {
    /// Host limits for child sessions. `None` (unset) is unlimited. Zero depth
    /// disables new children; zero concurrency or retention is rejected.
    #[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
    #[serde(default)]
    pub struct Agents {
        pub max_depth: Option<usize> => LIVE,
        pub max_active_per_parent: Option<usize> => LIVE,
        pub max_active_total: Option<usize> => LIVE,
        pub max_retained: Option<usize> => LIVE,
        /// Running-child inactivity interval. Zero disables host reminders.
        pub stale_reminder_secs: u64 => LIVE,
        /// Idle completion delivery; busy request notices remain available when off.
        pub auto_wake: bool => LIVE,
        pub wake_debounce_ms: u64 => LIVE,
        /// Failed/cancelled/timed-out internal-worker chat retention.
        pub worker_retention_days: u32 => LIVE,
        /// Preserve every worker transcript, including acknowledged successes.
        pub worker_keep_all: bool => LIVE,
    }
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
    pub(super) fn validate(&self) -> Result<()> {
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
config_section! {
    /// An empty path selects the workspace-scoped default; file paths resolve at their layer.
    #[derive(Clone, Debug, Deserialize)]
    pub struct StorageSettings {
        pub data_dir: PathBuf => RESTART,
        pub finished_event_retention_hours: u32 => RESTART,
        pub telemetry_retention_days: u32 => RESTART,
        pub archive_after_days: u32 => RESTART,
        /// None keeps archived chats forever. Applied only when the host starts.
        pub archived_retention_days: Option<u32> => RESTART,
        pub cold_tail_messages: u32 => RESTART,
        pub cold_min_messages: u32 => RESTART,
        pub cold_min_bytes: u64 => RESTART,
        pub cold_row_min_bytes: u32 => RESTART,
    }
}
config_section! {
    /// Command execution limits; see docs/exec.md. An unset cap means no limit.
    #[derive(Clone, Debug, Deserialize)]
    #[serde(default)]
    pub struct ExecConfig {
        /// Sync `exec` calls move to background after this many seconds; 0 backgrounds at once.
        pub sync_timeout_secs: u64 => LIVE,
        pub max_jobs_per_session: Option<usize> => LIVE,
        pub max_jobs: Option<usize> => LIVE,
        /// Per-job output spool cap in bytes.
        pub max_output_bytes: usize => LIVE,
    }
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
    pub(super) fn validate(&self) -> Result<()> {
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
config_section! {
    /// `bubblepaw delegate` model classes. Categories, the explore share and other
    /// delegation policy are constants in the delegate module, not configuration.
    #[derive(Clone, Debug, Default, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct DelegateSettings {
        #[serde(default)]
        pub classes: BTreeMap<String, ModelClass> => Node::Map(&Node::Table(ModelClass::FIELDS)),
    }
}
config_section! {
    /// One model class: its default and the other candidates, each `provider/model`.
    #[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ModelClass {
        pub default: String => RESTART,
        #[serde(default)]
        pub candidates: Vec<String> => RESTART,
    }
}
impl DelegateSettings {
    pub(super) fn validate(&self) -> Result<()> {
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
config_section! {
    /// File-safe web settings: only the key's environment name is stored/rendered.
    #[derive(Clone, Debug, Deserialize)]
    pub struct WebSettings {
        pub enabled: bool => LIVE,
        /// Ordered backends; a single string is a one-entry list.
        #[serde(deserialize_with = "one_or_many")]
        pub search: Vec<String> => LIVE,
        #[serde(deserialize_with = "one_or_many")]
        pub fetch_backend: Vec<String> => LIVE,
        pub key_env: String => LIVE,
        pub deepseek_key_env: String => LIVE,
        pub deepseek_base_url: String => LIVE,
        /// Additional key variable names permitted by web provider setup.
        pub provider_key_envs: Vec<String> => LIVE,
        pub allow_private: bool => LIVE,
        pub user_agent: String => LIVE,
        pub cache_ttl_secs: u64 => RESTART,
        pub cache_max_bytes: usize => RESTART,
        #[serde(default)]
        pub max_searches: Option<usize> => LIVE,
        #[serde(default)]
        pub max_fetches: Option<usize> => LIVE,
        pub cooldown_secs: u64 => LIVE,
        pub expose: WebExpose => Node::Table(WebExpose::FIELDS),
    }
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
    pub(super) fn validate(&self) -> Result<()> {
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

config_section! {
    #[derive(Clone, Debug, Deserialize)]
    pub struct ApprovalSettings {
        pub confirm: String => LIVE,
        #[serde(default)]
        pub tools: BTreeMap<String, String> => Node::Map(&LIVE),
        #[serde(default)]
        pub deny: Vec<String> => LIVE,
        #[serde(default)]
        pub read_only: bool => LIVE,
        /// The fixed exec blocklist (docs/approvals.md). On unless a config turns it off.
        #[serde(default = "default_hardline")]
        pub hardline: bool => LIVE,
        /// Approver prompt template; None uses `bubblepaw_core::DEFAULT_AUTO_PROMPT`.
        pub auto_prompt: Option<String> => LIVE,
    }
}
fn default_hardline() -> bool {
    true
}
config_section! {
    #[derive(Clone, Debug, Deserialize)]
    pub struct ProviderSettings {
        pub kind: Option<String> => LIVE,
        /// Stable quota identity; omission uses this provider entry's name.
        pub account: Option<String> => LIVE,
        pub base_url: Option<String> => LIVE,
        pub key_env: Option<String> => LIVE,
        pub auth_file: Option<PathBuf> => LIVE,
        #[serde(default)]
        pub reasoning: BTreeMap<String, Value> => REASONING,
        /// Per-model settings. Model IDs may contain dots, so they are values here,
        /// not table keys.
        #[serde(default)]
        pub models: Vec<ProviderModelSettings> => LIVE,
    }
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
config_section! {
    /// Root harness settings, resolved at host startup. None retains the runner's instructions.
    #[derive(Clone, Debug, Deserialize)]
    pub struct RootAgentSettings {
        pub system_prompt: Option<String> => RESTART,
        pub max_rounds: Option<usize> => RESTART,
        #[serde(default)]
        pub execution_mode: ExecutionMode => RESTART,
    }
}
config_section! {
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(default)]
    pub struct WebExpose {
        pub fetch: bool => LIVE,
        pub search: bool => LIVE,
    }
}
impl Default for WebExpose {
    fn default() -> Self {
        Self {
            fetch: true,
            search: true,
        }
    }
}
