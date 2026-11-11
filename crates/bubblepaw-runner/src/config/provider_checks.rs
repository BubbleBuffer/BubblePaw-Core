//! Provider setup validation, accounts and endpoint resolution.
use super::*;

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
pub(super) fn valid_id(name: &str) -> bool {
    !name.is_empty() && !name.contains(['/', '.']) && !name.chars().any(char::is_whitespace)
}
pub(super) fn validate_account(account: Option<&str>, path: &str) -> Result<()> {
    ensure!(
        account.is_none_or(|account| !account.is_empty()
            && account.len() <= 256
            && account.trim() == account
            && !account.chars().any(char::is_control)),
        "{path}.account must contain 1..=256 bytes without surrounding whitespace or control characters"
    );
    Ok(())
}
config_section! {
    #[derive(Deserialize)]
    pub(super) struct RawSettings {
        pub(super) model: ModelSettings => Node::Table(ModelSettings::FIELDS),
        pub(super) agent: RootAgentSettings => Node::Table(RootAgentSettings::FIELDS),
        pub(super) storage: StorageSettings => Node::Table(StorageSettings::FIELDS),
        pub(super) exec: ExecConfig => Node::Table(ExecConfig::FIELDS),
        pub(super) quota: QuotaSettings => Node::Table(QuotaSettings::FIELDS),
        pub(super) remote: RemoteSettings => Node::Table(RemoteSettings::FIELDS),
        pub(super) providers: BTreeMap<String, ProviderSettings> => Node::Map(&Node::Table(ProviderSettings::FIELDS)),
        pub(super) roles: BTreeMap<String, RoleEntry> => Node::Table(roles::ROLE_FIELDS),
        pub(super) context: ContextSettings => Node::Table(ContextSettings::FIELDS),
        pub(super) skills: SkillsConfig => skills_section::SKILLS,
        #[serde(default)] pub(super) toolsets: BTreeMap<String, Toolset> => Node::Map(&Node::Table(Toolset::FIELDS)),
        pub(super) approvals: ApprovalSettings => Node::Table(ApprovalSettings::FIELDS),
        pub(super) agents: Agents => Node::Table(Agents::FIELDS),
        pub(super) web: WebSettings => Node::Table(WebSettings::FIELDS),
        pub(super) extensions: BTreeMap<String, ExtensionSettings> => Node::Map(&Node::Table(ExtensionSettings::FIELDS)),
        pub(super) mcp: McpSettings => Node::Table(McpSettings::FIELDS),
        pub(super) delegate: DelegateSettings => Node::Table(DelegateSettings::FIELDS),
        #[allow(dead_code)] pub(super) workspace_trust: crate::workspace_trust::TrustSettings => Node::Table(crate::workspace_trust::TrustSettings::FIELDS),
    }
}
config_section! {
    #[derive(Deserialize)]
    pub(super) struct ModelSettings {
        pub(super) main: String => LIVE,
        pub(super) provider: String => LIVE,
        #[serde(default)] pub(super) fallbacks: Vec<String> => LIVE,
    }
}
config_section! {
    #[derive(Deserialize)]
    pub(super) struct ContextSettings {
        pub(super) strategy: String => RESTART,
        pub(super) memory_files: bool => RESTART,
        pub(super) compact_threshold_tokens: usize => LIVE,
        pub(super) window_tokens: Option<usize> => LIVE,
        pub(super) auto_summarize: bool => LIVE,
        pub(super) retry_base_ms: u64 => RESTART,
        pub(super) classical: ClassicalSettings => Node::Table(ClassicalSettings::FIELDS),
        #[serde(default)] pub(super) lcm: crate::summarization::LcmConfig => Node::Table(crate::summarization::LcmConfig::FIELDS),
    }
}
config_section! {
    #[derive(Deserialize)]
    pub(super) struct ClassicalSettings {
        pub(super) verbatim_user_tokens: usize => RESTART,
    }
}
#[derive(Clone, Deserialize)]
#[serde(untagged)]
pub(super) enum RoleEntry {
    Model(String),
    Models(Vec<String>),
    Table(RoleTable),
}
config_section! {
    #[derive(Clone, Deserialize)]
    pub(super) struct RoleTable {
        pub(super) models: Vec<String> => LIVE,
        pub(super) reasoning_effort: Option<ReasoningEffort> => LIVE,
        pub(super) max_output_tokens: Option<u32> => LIVE,
        pub(super) repeats: Option<usize> => LIVE,
        pub(super) threshold: Option<f64> => LIVE,
    }
}
impl RoleEntry {
    pub(super) fn models(&self) -> Vec<String> {
        match self {
            Self::Model(model) => vec![model.clone()],
            Self::Models(models) => models.clone(),
            Self::Table(entry) => entry.models.clone(),
        }
    }
}

pub(super) fn endpoints(
    providers: &BTreeMap<String, ProviderSettings>,
) -> Result<ProviderEndpoints> {
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
