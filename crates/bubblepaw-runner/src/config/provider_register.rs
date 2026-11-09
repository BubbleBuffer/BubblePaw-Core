//! Registering the configured provider transports: which adapter each `providers.*`
//! entry becomes, and where it sends its traffic.
use super::*;

impl Settings {
    /// The configured transports, using stored keys before `environment`.
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
            // Model traffic through a paired device, when `via` names one.
            let egress = self.egress_for(name)?;
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
                    if let Some(egress) = &egress {
                        adapter = adapter.with_egress(egress.clone())?;
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
                continue;
            }
            let key = self.provider_key(name, environment)?;
            if kind == "chat-completions"
                && provider.key_env.is_none()
                && key.is_none()
                && crate::provider::is_loopback_url(base)
            {
                let adapter = ChatCompletionsProvider::without_key(base.clone())?
                    .with_reasoning_fields(serde_json::to_value(&provider.reasoning)?)?
                    .with_reasoning_replay(provider.reasoning_replay())?;
                if registry.get(name).is_none() {
                    registry.insert(name, Arc::new(adapter))?;
                }
                continue;
            }
            let Some(key) = key else {
                continue;
            };
            ensure!(
                reqwest::header::HeaderValue::from_str(&key).is_ok(),
                "Invalid credential header for provider {name}"
            );
            match kind {
                "anthropic-messages" if registry.get(name).is_none() => {
                    let mut adapter = MessagesProvider::api_key(key, base.clone())?;
                    if let Some(egress) = &egress {
                        adapter = adapter.with_egress(egress.clone())?;
                    }
                    if is_opencode_route(name) {
                        adapter = adapter.with_opencode_route();
                    }
                    registry.insert(name, Arc::new(adapter))?;
                }
                "chat-completions" if registry.get(name).is_none() => {
                    let mut adapter = ChatCompletionsProvider::api_key(key, base.clone())?
                        .with_reasoning_fields(serde_json::to_value(&provider.reasoning)?)?
                        .with_reasoning_replay(provider.reasoning_replay())?;
                    if let Some(egress) = &egress {
                        adapter = adapter.with_egress(egress.clone())?;
                    }
                    if is_opencode_route(name) {
                        adapter = adapter.with_opencode_route();
                    }
                    registry.insert(name, Arc::new(adapter))?;
                }
                "responses" if registry.get(name).is_none() => {
                    let mut adapter = ResponsesProvider::api_key(key, base.clone())?;
                    if let Some(egress) = &egress {
                        adapter = adapter.with_egress(egress.clone())?;
                    }
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
