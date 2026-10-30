//! Live settings classification and projection over retained startup resources.
use super::*;

impl EffectiveConfig {
    /// Compare only settings that can be published without restarting the host.
    pub fn same_provider_config(&self, other: &Self) -> bool {
        Self::live_paths()
            .iter()
            .all(|path| self.value(path) == other.value(path))
    }
    /// Detect edits to settings whose runtime resources remain fixed at startup.
    pub fn restart_needed(&self, startup: &Self) -> bool {
        let mut current = self.value.clone();
        let mut original = startup.value.clone();
        for path in Self::live_paths() {
            let parts: Vec<_> = path.split('.').collect();
            // The paths are known settings; replacing them cannot fail.
            merge_at(&mut current, &parts, Value::String(String::new())).expect("known live path");
            merge_at(&mut original, &parts, Value::String(String::new())).expect("known live path");
        }
        current != original
    }
    pub(super) fn live_paths() -> &'static [&'static str] {
        &[
            "model",
            "providers",
            "roles",
            "approvals",
            "agents",
            "exec",
            "mcp",
            "skills",
            "context.window_tokens",
            "context.compact_threshold_tokens",
            "context.auto_summarize",
            "web.enabled",
            "web.expose",
            "web.search",
            "web.fetch_backend",
            "web.key_env",
            "web.deepseek_key_env",
            "web.deepseek_base_url",
            "web.cooldown_secs",
            "web.provider_key_envs",
            "web.allow_private",
            "web.user_agent",
            "web.max_searches",
            "web.max_fetches",
        ]
    }
}

impl Settings {
    /// Retain resources/policy created at startup while replacing live routing.
    pub fn with_live_provider_settings(&self, candidate: Settings) -> Self {
        let mut settings = self.clone();
        settings.main_model = candidate.main_model;
        settings.main_provider = candidate.main_provider;
        settings.main_fallbacks = candidate.main_fallbacks;
        settings.roles = candidate.roles;
        settings.endpoints = candidate.endpoints;
        settings.providers = candidate.providers;
        settings.configured_providers = candidate.configured_providers;
        settings.approvals = candidate.approvals;
        settings.agents = candidate.agents;
        settings.exec = candidate.exec;
        settings.mcp = candidate.mcp;
        settings.skills = candidate.skills;
        settings.context_window_tokens = candidate.context_window_tokens;
        settings.compact_threshold_tokens = candidate.compact_threshold_tokens;
        settings.auto_summarize = candidate.auto_summarize;
        settings.web.enabled = candidate.web.enabled;
        settings.web.expose = candidate.web.expose;
        settings.web.search = candidate.web.search;
        settings.web.fetch_backend = candidate.web.fetch_backend;
        settings.web.key_env = candidate.web.key_env;
        settings.web.deepseek_key_env = candidate.web.deepseek_key_env;
        settings.web.deepseek_base_url = candidate.web.deepseek_base_url;
        settings.web.cooldown_secs = candidate.web.cooldown_secs;
        settings.web.provider_key_envs = candidate.web.provider_key_envs;
        settings.web.allow_private = candidate.web.allow_private;
        settings.web.user_agent = candidate.web.user_agent;
        settings.web.max_searches = candidate.web.max_searches;
        settings.web.max_fetches = candidate.web.max_fetches;
        settings
    }
}
