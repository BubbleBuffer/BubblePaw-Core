//! Non-generating provider checks behind the test route. Probes read the
//! models list or an authentication endpoint; they never call a model.
use super::input::{ProviderInput, validate_setup};
use super::{ApiError, ApiResult, ProviderSetup, bad, io_error, valid_name};
use crate::config::{EffectiveConfig, ProviderSettings};
use anyhow::Result;
use futures_util::StreamExt;
use reqwest::StatusCode;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::PathBuf,
    time::{Duration, Instant},
};

const BODY_CAP: usize = 2 * 1024 * 1024;

#[derive(Serialize)]
struct ProbeResult {
    config: ConfigCheck,
    reachable: Option<bool>,
    auth: &'static str,
    auth_detail: String,
    models_listed: Option<Vec<String>>,
    configured_models: Vec<Value>,
    detail: String,
    elapsed_ms: u128,
}
#[derive(Serialize)]
struct ConfigCheck {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}
impl ProbeResult {
    fn invalid(message: &str) -> Self {
        Self {
            config: ConfigCheck {
                ok: false,
                error: Some(message.into()),
            },
            reachable: None,
            auth: "unchecked",
            auth_detail: "configuration is invalid".into(),
            models_listed: None,
            configured_models: Vec::new(),
            detail: "Configuration check failed".into(),
            elapsed_ms: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConfirmedHost {
    OpenAi,
    Anthropic,
    OpenRouter,
    OpenCode,
}
/// Exact kind/host pairs with documented non-generating probe endpoints.
pub(crate) fn confirmed_host(kind: &str, base: &reqwest::Url) -> Option<ConfirmedHost> {
    match (
        kind,
        base.scheme(),
        base.host_str(),
        base.port_or_known_default(),
    ) {
        (
            "responses" | "chat-completions" | "decisions" | "openai-decisions",
            "https",
            Some("api.openai.com"),
            Some(443),
        ) => Some(ConfirmedHost::OpenAi),
        ("anthropic-messages", "https", Some("api.anthropic.com"), Some(443)) => {
            Some(ConfirmedHost::Anthropic)
        }
        ("openrouter-decisions", "https", Some("openrouter.ai"), Some(443)) => {
            Some(ConfirmedHost::OpenRouter)
        }
        (
            "responses"
            | "chat-completions"
            | "anthropic-messages"
            | "decisions"
            | "openai-decisions"
            | "openrouter-decisions",
            "https",
            Some("opencode.ai"),
            _,
        ) => Some(ConfirmedHost::OpenCode),
        _ => None,
    }
}
pub(crate) fn model_url(
    base: &reqwest::Url,
    confirmed: Option<ConfirmedHost>,
) -> Result<reqwest::Url> {
    match confirmed {
        Some(ConfirmedHost::OpenAi) => Ok(reqwest::Url::parse("https://api.openai.com/v1/models")?),
        Some(ConfirmedHost::OpenRouter) => {
            Ok(reqwest::Url::parse("https://openrouter.ai/api/v1/key")?)
        }
        Some(ConfirmedHost::OpenCode) => {
            Ok(reqwest::Url::parse(if base.path().contains("/go/") {
                "https://opencode.ai/inference/go/v1/models"
            } else {
                "https://opencode.ai/inference/v1/models"
            })?)
        }
        _ => {
            let mut url = base.clone();
            url.set_path(&format!("{}/models", base.path().trim_end_matches('/')));
            if confirmed == Some(ConfirmedHost::Anthropic) {
                url.set_query(Some("limit=1000"));
            }
            Ok(url)
        }
    }
}
pub(crate) fn auth_status(
    confirmed: Option<ConfirmedHost>,
    status: reqwest::StatusCode,
) -> (&'static str, &'static str) {
    if matches!(status.as_u16(), 401 | 403) {
        ("failed", "endpoint rejected authentication")
    } else if confirmed == Some(ConfirmedHost::OpenCode) {
        (
            "unchecked",
            "models list documented; auth not confirmed for this endpoint",
        )
    } else if confirmed.is_none() {
        ("unchecked", "endpoint not confirmed for this host")
    } else if status.is_success() {
        ("ok", "confirmed authentication endpoint returned success")
    } else {
        ("unchecked", "authentication was not confirmed")
    }
}
pub(crate) fn codex_auth_file(
    provider: &ProviderSettings,
    config: &EffectiveConfig,
) -> Option<PathBuf> {
    provider
        .auth_file
        .clone()
        .or_else(|| {
            config
                .environment("CODEX_HOME")
                .map(|path| PathBuf::from(path).join("auth.json"))
        })
        .or_else(|| {
            config
                .environment("USERPROFILE")
                .or_else(|| config.environment("HOME"))
                .map(|path| PathBuf::from(path).join(".codex/auth.json"))
        })
}
async fn probe(name: &str, provider: &ProviderSettings, config: &EffectiveConfig) -> ProbeResult {
    let start = Instant::now();
    if provider.validate(name).is_err() {
        return ProbeResult::invalid("Invalid provider configuration");
    }
    let kind = provider.kind.as_deref().unwrap_or("chat-completions");
    let mut result = ProbeResult {
        config: ConfigCheck {
            ok: true,
            error: None,
        },
        reachable: None,
        auth: "unchecked",
        auth_detail: "no non-generating check".into(),
        models_listed: None,
        configured_models: provider
            .models
            .iter()
            .map(|model| json!({"id": model.id, "listed": null}))
            .collect(),
        detail: "Configuration is valid".into(),
        elapsed_ms: 0,
    };
    if kind == "scripted" {
        result.auth = "not_applicable";
        result.auth_detail = "scripted provider has no authentication".into();
        result.elapsed_ms = start.elapsed().as_millis();
        return result;
    }
    let key = if kind == "codex" {
        None
    } else {
        provider
            .key_env
            .as_ref()
            .and_then(|name| config.environment(name))
            .filter(|key| !key.is_empty())
    };
    if kind != "codex" && key.is_none() {
        result.config = ConfigCheck {
            ok: false,
            error: Some(format!(
                "{} is not set in the invoking environment",
                provider.key_env.as_deref().unwrap_or("key_env")
            )),
        };
    }
    if kind == "codex" {
        // Reuse the transport's parser; constructing it makes no network request.
        if codex_auth_file(provider, config).is_none_or(|path| {
            crate::provider::ResponsesProvider::codex_auth(
                path,
                provider.base_url.clone().unwrap_or_default(),
            )
            .is_err()
        }) {
            result.config = ConfigCheck {
                ok: false,
                error: Some(
                    "Codex auth file is missing, invalid, or lacks an access token/account id"
                        .into(),
                ),
            };
        }
    }
    let base = match provider
        .base_url
        .as_deref()
        .and_then(|base| reqwest::Url::parse(base).ok())
    {
        Some(base) => base,
        None => {
            result.config = ConfigCheck {
                ok: false,
                error: Some("Provider needs a valid base_url".into()),
            };
            return result;
        }
    };
    let confirmed = if kind == "codex" {
        None
    } else {
        confirmed_host(kind, &base)
    };
    result.auth_detail = if kind == "codex" {
        "no non-generating check"
    } else if confirmed == Some(ConfirmedHost::OpenCode) {
        "models list documented; auth not confirmed for this endpoint"
    } else if confirmed.is_none() {
        "endpoint not confirmed for this host"
    } else {
        "authentication endpoint has not answered"
    }
    .into();
    let url = if kind == "codex" {
        let mut root = base.clone();
        root.set_path("/");
        Ok(root)
    } else {
        model_url(&base, confirmed)
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build();
    let outcome = async {
        let mut request = client
            .map_err(|_| "Could not build the HTTP client")?
            .get(url.map_err(|_| "Invalid probe URL")?);
        if kind != "codex" {
            if kind == "anthropic-messages" {
                request = request.header("anthropic-version", "2023-06-01");
                if let Some(key) = &key {
                    request = request.header("x-api-key", key);
                }
            } else if let Some(key) = &key {
                request = request.bearer_auth(key);
            }
        }
        let response = request.send().await.map_err(|error| {
            if error.is_timeout() {
                "Host did not answer within the probe limit"
            } else if error.is_connect() {
                "Could not connect to the host"
            } else {
                "Request failed before a response"
            }
        })?;
        let status = response.status();
        result.reachable = Some(true);
        result.detail = format!("HTTP {}", status.as_u16());
        if kind != "codex" {
            let (auth, detail) = auth_status(confirmed, status);
            result.auth = auth;
            result.auth_detail = detail.into();
        }
        if status.is_success() && kind != "codex" && confirmed != Some(ConfirmedHost::OpenRouter) {
            let mut stream = response.bytes_stream();
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| "Model list could not be read")?;
                if bytes.len().saturating_add(chunk.len()) > BODY_CAP {
                    result
                        .detail
                        .push_str("; model list exceeded the response limit");
                    return Ok(());
                }
                bytes.extend_from_slice(&chunk);
            }
            if let Ok(body) = serde_json::from_slice::<Value>(&bytes)
                && let Some(data) = body.get("data").and_then(Value::as_array)
            {
                let ids: Vec<_> = data
                    .iter()
                    .filter_map(|entry| entry.get("id").and_then(Value::as_str))
                    .filter(|id| !id.is_empty() && !id.chars().any(char::is_control))
                    // Never relay a credential echoed by an untrusted models endpoint.
                    .filter(|id| key.as_ref().is_none_or(|key| !id.contains(key)))
                    .map(str::to_owned)
                    .collect();
                let listed: HashSet<_> = ids.iter().map(String::as_str).collect();
                for model in &mut result.configured_models {
                    model["listed"] =
                        json!(model["id"].as_str().is_some_and(|id| listed.contains(id)));
                }
                result.models_listed = Some(ids.into_iter().take(500).collect());
            }
        }
        Ok::<_, &'static str>(())
    };
    let failure = match tokio::time::timeout(Duration::from_secs(10), outcome).await {
        Ok(Ok(())) => None,
        Ok(Err(reason)) => Some(reason),
        Err(_) => Some("Host did not answer within the probe limit"),
    };
    if let Some(reason) = failure {
        if result.reachable.is_none() {
            result.reachable = Some(false);
            result.detail = reason.into();
        } else {
            result.detail.push_str("; model list could not be read");
        }
    }
    result.elapsed_ms = start.elapsed().as_millis();
    result
}

impl ProviderSetup {
    /// Probe a saved provider without calling a generating endpoint.
    pub async fn test(&self, name: &str) -> Result<Value> {
        Ok(serde_json::to_value(self.probe_inner(name, None).await?)?)
    }
    /// Probe an unsaved draft under `name`. `draft` is the provider JSON that
    /// `save` accepts. A draft that fails validation is a 200 with `config.ok`
    /// false; a draft that is not provider JSON is a bad request.
    pub async fn test_draft(&self, name: &str, draft: Value) -> Result<Value> {
        Ok(serde_json::to_value(
            self.probe_inner(name, Some(draft)).await?,
        )?)
    }
    async fn probe_inner(&self, name: &str, draft: Option<Value>) -> ApiResult<ProbeResult> {
        if !valid_name(name) {
            return Err(bad());
        }
        let (provider, config) = {
            let _guard = self.0.files.lock().map_err(|_| io_error())?;
            let config = self.0.load()?;
            let settings = config.settings().map_err(|_| bad())?;
            let provider = match draft {
                Some(draft) => {
                    let draft: ProviderInput = serde_json::from_value(draft).map_err(|_| bad())?;
                    match draft.settings(name, &self.0.target) {
                        Ok(provider) => provider,
                        Err(_) => {
                            return Ok(ProbeResult::invalid("Invalid provider configuration"));
                        }
                    }
                }
                None => settings
                    .providers
                    .get(name)
                    .cloned()
                    .ok_or(ApiError(StatusCode::NOT_FOUND, "Unknown provider".into()))?,
            };
            validate_setup(&provider, &settings.web.provider_key_envs)?;
            if provider.validate(name).is_err() {
                return Ok(ProbeResult::invalid("Invalid provider configuration"));
            }
            (provider, config)
        };
        Ok(probe(name, &provider, &config).await)
    }
}
