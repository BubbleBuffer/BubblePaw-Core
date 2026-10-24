//! Built-in adapter setup. Hosts provide roles separately from transport settings.
use super::{
    ChatCompletionsProvider, MessagesProvider, OpenAiDecisions, OpenRouterDecisions,
    ProviderRegistry,
};
use anyhow::{Result, ensure};
use bubblepaw_core::ReasoningReplay;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Debug)]
pub struct ProviderEndpoints {
    pub opencode_base_url: Option<String>,
    pub opencode_key_env: String,
    pub opencode_reasoning_fields: Value,
    /// Exact chat model ID to reasoning replay policy; absent models use `tool_loop`.
    pub opencode_reasoning_replay: BTreeMap<String, ReasoningReplay>,
    pub opencode_messages_base_url: Option<String>,
    pub opencode_messages_key_env: String,
    pub openrouter_decisions_url: String,
    pub openrouter_key_env: String,
    pub openai_base_url: String,
    pub openai_key_env: String,
}
impl Default for ProviderEndpoints {
    fn default() -> Self {
        Self {
            opencode_base_url: Some("https://opencode.ai/zen/go/v1".into()),
            opencode_key_env: "OPENCODE_API_KEY".into(),
            opencode_reasoning_fields: json!({"thinking":{"type":"enabled"},"reasoning_effort":"low"}),
            opencode_reasoning_replay: BTreeMap::new(),
            opencode_messages_base_url: Some("https://opencode.ai/zen/go/v1".into()),
            opencode_messages_key_env: "OPENCODE_API_KEY".into(),
            openrouter_decisions_url: "https://openrouter.ai/api/alpha/decisions".into(),
            openrouter_key_env: "OPENROUTER_API_KEY".into(),
            openai_base_url: "https://api.openai.com".into(),
            openai_key_env: "OPENAI_API_KEY".into(),
        }
    }
}
/// Explicit keys isolate fixtures and allow a host's local metering relay.
#[derive(Default)]
pub struct ProviderKeys {
    pub opencode: Option<String>,
    pub openrouter: Option<String>,
    pub openai: Option<String>,
}
impl ProviderEndpoints {
    pub fn validate(&self) -> Result<()> {
        if let Some(base) = &self.opencode_base_url {
            super::decisions::validate_url(base)?;
        }
        if let Some(base) = &self.opencode_messages_base_url {
            super::decisions::validate_url(base)?;
        }
        super::decisions::validate_url(&self.openrouter_decisions_url)?;
        super::decisions::validate_url(&self.openai_base_url)?;
        for name in [
            &self.opencode_key_env,
            &self.opencode_messages_key_env,
            &self.openrouter_key_env,
            &self.openai_key_env,
        ] {
            ensure!(
                !name.is_empty() && !name.contains(['=', '\0']),
                "Invalid provider key environment name"
            );
        }
        ChatCompletionsProvider::validate_reasoning_fields(&self.opencode_reasoning_fields)?;
        ChatCompletionsProvider::validate_reasoning_replay(&self.opencode_reasoning_replay)?;
        Ok(())
    }
    pub fn register(&self, registry: &mut ProviderRegistry) -> Result<()> {
        self.validate()?;
        self.register_keys(
            registry,
            ProviderKeys {
                opencode: std::env::var(&self.opencode_key_env).ok(),
                openrouter: std::env::var(&self.openrouter_key_env).ok(),
                openai: std::env::var(&self.openai_key_env).ok(),
            },
            std::env::var(&self.opencode_messages_key_env).ok(),
        )
    }
    pub fn register_with_keys(
        &self,
        registry: &mut ProviderRegistry,
        keys: ProviderKeys,
    ) -> Result<()> {
        // Explicit Go fixtures/relays share the OpenCode credential. Configured
        // transports registered from the environment use their own key_env.
        let messages_key = keys.opencode.clone();
        self.register_keys(registry, keys, messages_key)
    }
    fn register_keys(
        &self,
        registry: &mut ProviderRegistry,
        keys: ProviderKeys,
        messages_key: Option<String>,
    ) -> Result<()> {
        self.validate()?;
        if let (Some(base), Some(key)) = (
            &self.opencode_messages_base_url,
            messages_key.filter(|key| !key.is_empty()),
        ) && registry.get("opencode-messages").is_none()
        {
            registry.insert(
                "opencode-messages",
                Arc::new(MessagesProvider::api_key(key, base.clone())?.with_opencode_route()),
            )?;
        }
        if let (Some(base), Some(key)) = (
            &self.opencode_base_url,
            keys.opencode.filter(|key| !key.is_empty()),
        ) && registry.get("opencode").is_none()
        {
            registry.insert(
                "opencode",
                Arc::new(
                    ChatCompletionsProvider::api_key(key, base.clone())?
                        .with_reasoning_fields(self.opencode_reasoning_fields.clone())?
                        .with_reasoning_replay(self.opencode_reasoning_replay.clone())?
                        .with_opencode_route(),
                ),
            )?;
        }
        if let Some(key) = keys.openrouter.filter(|key| !key.is_empty())
            && registry.get_decision("openrouter").is_none()
        {
            registry.insert_decision(
                "openrouter",
                Arc::new(OpenRouterDecisions::new(
                    key,
                    self.openrouter_decisions_url.clone(),
                )?),
            )?;
        }
        if let Some(key) = keys.openai.filter(|key| !key.is_empty())
            && registry.get_decision("openai").is_none()
        {
            registry.insert_decision(
                "openai",
                Arc::new(OpenAiDecisions::new(key, self.openai_base_url.clone())?),
            )?;
        }
        Ok(())
    }
}
