//! The provider behind the main model's route, or the explicit absence of one.
//! A route with no configured provider or no usable credentials is not a
//! startup failure: the host starts, answers the configuration methods and
//! refuses model work with [`NoProviderConfigured`] until a candidate that
//! resolves is applied.
use crate::provider::{
    ChatCompletionsProvider, MessagesProvider, ResponsesProvider, ScriptedProvider,
};
use crate::{
    config::{Settings, is_opencode_route, selected_main_provider},
    no_provider::NoProviderConfigured,
};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ModelRequest, ModelResponse, Provider, ProviderError, ProviderErrorKind,
    TextCallback,
};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

/// The main route's provider and, when the route cannot be served, why.
pub(super) type DefaultRoute = (Arc<dyn Provider>, Option<NoProviderConfigured>);

/// The slot an unavailable route occupies in the registry. It makes every
/// route lookup resolve as usual and refuses each model call with the typed
/// reason; a published candidate replaces the whole registry.
struct UnavailableProvider(NoProviderConfigured);

#[async_trait]
impl Provider for UnavailableProvider {
    async fn generate(
        &self,
        _request: ModelRequest,
        _on_text: TextCallback,
        _cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        Err(ProviderError::new(ProviderErrorKind::Fatal, self.0.to_string()).into())
    }
}

pub(super) fn default_route(
    settings: &Settings,
    extensions: &BTreeMap<String, Arc<dyn Provider>>,
    environment: &dyn Fn(&str) -> Option<String>,
) -> Result<DefaultRoute> {
    let default = selected_main_provider(settings);
    if let Some(extension) = settings
        .extensions
        .iter()
        .find(|extension| extension.id == default)
    {
        let provider = extensions
            .get(&extension.id)
            .context("Missing startup extension")?;
        return Ok((provider.clone(), None));
    }
    let missing = |reason: String| -> Result<DefaultRoute> {
        let unavailable = NoProviderConfigured::new(default, reason);
        Ok((
            Arc::new(UnavailableProvider(unavailable.clone())) as Arc<dyn Provider>,
            Some(unavailable),
        ))
    };
    // A route named by the config but absent from `providers` is a mistake in
    // the config, not a missing credential.
    let transport = settings.providers.get(default).with_context(|| {
        format!("model.provider selects {default}, which has no providers.{default} table")
    })?;
    let base = transport.base_url.clone();
    let kind = transport.kind.as_deref().unwrap_or("chat-completions");
    if kind != "scripted" && base.as_deref().is_none_or(|base| base.trim().is_empty()) {
        return missing(format!(
            "providers.{default}.base_url is empty, which disables the route"
        ));
    }
    let provider: Arc<dyn Provider> = match kind {
        "scripted" => Arc::new(ScriptedProvider::demo(!settings.approvals.read_only)),
        "codex" => {
            let auth = transport
                .auth_file
                .clone()
                .or_else(|| environment("CODEX_HOME").map(|p| PathBuf::from(p).join("auth.json")))
                .or_else(|| {
                    environment("USERPROFILE")
                        .or_else(|| environment("HOME"))
                        .map(|p| PathBuf::from(p).join(".codex/auth.json"))
                });
            let Some(auth) = auth else {
                return missing(format!(
                    "providers.{default} has no Codex auth file; set providers.{default}.auth_file"
                ));
            };
            let base = base.context("Codex provider needs base_url")?;
            match ResponsesProvider::codex_auth(&auth, base) {
                Ok(mut provider) => {
                    if let Some(effort) = transport
                        .reasoning
                        .get("reasoning_effort")
                        .and_then(toml::Value::as_str)
                    {
                        provider = provider.with_reasoning_effort(effort);
                    }
                    Arc::new(provider)
                }
                Err(error) => {
                    return missing(format!(
                        "the Codex auth file for providers.{default} is unusable ({error:#}); \
                         set providers.{default}.auth_file"
                    ));
                }
            }
        }
        "responses" | "chat-completions" | "anthropic-messages" => {
            let Some(key_env) = transport.key_env.as_ref() else {
                return missing(format!(
                    "providers.{default}.key_env does not name a key variable"
                ));
            };
            let Some(key) = environment(key_env) else {
                return missing(format!(
                    "the variable {key_env} named by providers.{default}.key_env is not set"
                ));
            };
            anyhow::ensure!(
                reqwest::header::HeaderValue::from_str(&key).is_ok(),
                "Invalid credential header for provider {default}"
            );
            let base = base.context("Provider needs base_url")?;
            if kind == "responses" {
                let mut provider = ResponsesProvider::api_key(key, base)?;
                if is_opencode_route(default) {
                    provider = provider.with_opencode_route();
                }
                if let Some(effort) = transport
                    .reasoning
                    .get("reasoning_effort")
                    .and_then(toml::Value::as_str)
                {
                    provider = provider.with_reasoning_effort(effort);
                }
                Arc::new(provider)
            } else if kind == "anthropic-messages" {
                let mut provider = MessagesProvider::api_key(key, base)?;
                if is_opencode_route(default) {
                    provider = provider.with_opencode_route();
                }
                Arc::new(provider)
            } else {
                let mut provider = ChatCompletionsProvider::api_key(key, base)?
                    .with_reasoning_fields(serde_json::to_value(&transport.reasoning)?)?
                    .with_reasoning_replay(transport.reasoning_replay())?;
                if is_opencode_route(default) {
                    provider = provider.with_opencode_route();
                }
                Arc::new(provider)
            }
        }
        _ => bail!("model.provider must select an LLM provider"),
    };
    Ok((provider, None))
}
