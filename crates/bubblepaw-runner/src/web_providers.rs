//! Provider setup and live apply. Merge this router before the host's local boundary.
use crate::config::{EffectiveConfig, ProviderModelSettings, ProviderSettings};
pub use crate::provider_setup::ProviderSetup;
use crate::provider_setup::*;
use anyhow::{Result, ensure};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        Path, State,
        rejection::{BytesRejection, JsonRejection},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path as FilePath, PathBuf},
    time::{Duration, Instant},
};
use toml_edit::{DocumentMut, Item, Table, value};

const BODY_CAP: usize = 2 * 1024 * 1024;

/// Own state is already supplied; the returned router can be merged into any host.
pub fn router(state: ProviderSetup) -> Router {
    Router::new()
        .route("/api/providers", get(list))
        .route("/api/providers/{name}", put(save).delete(remove))
        .route("/api/roles/main", put(save_main))
        .route("/api/providers/{name}/test", post(test))
        .route("/api/config/reload", post(reload))
        .with_state(state)
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}
fn body<T>(input: std::result::Result<Json<T>, JsonRejection>) -> ApiResult<T> {
    input.map(|Json(input)| input).map_err(|_| bad())
}
fn valid_env(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && name
            .bytes()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}
fn loopback_url(base: &reqwest::Url) -> bool {
    base.host_str().is_some_and(|host| {
        host == "localhost"
            || host.ends_with(".localhost")
            || host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}
/// Apply before reading any credential, including for providers loaded from disk.
fn validate_setup(provider: &ProviderSettings, allowed_key_envs: &[String]) -> ApiResult<()> {
    if let Some(base) = &provider.base_url {
        let base = reqwest::Url::parse(base).map_err(|_| bad())?;
        if !matches!(base.scheme(), "http" | "https")
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(bad());
        }
        if base.scheme() != "https" && !loopback_url(&base) {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "base_url must use https unless it points at this machine (localhost/127.0.0.1/::1)"
                    .into(),
            ));
        }
    }
    if let Some(name) = &provider.key_env {
        if !valid_env(name) {
            return Err(bad());
        }
        if !["_API_KEY", "_KEY", "_TOKEN"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
            && !allowed_key_envs.contains(name)
        {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!(
                    "key_env {name} is not an allowed key variable; names must end in _API_KEY, _KEY or _TOKEN, or be listed in [web] provider_key_envs"
                ),
            ));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderInput {
    kind: String,
    base_url: Option<String>,
    key_env: Option<String>,
    auth_file: Option<PathBuf>,
    #[serde(default)]
    reasoning: BTreeMap<String, Value>,
    models: Vec<ProviderModelSettings>,
}
impl ProviderInput {
    fn settings(mut self, name: &str, target: &FilePath) -> Result<ProviderSettings> {
        ensure!(valid_name(name), "Invalid provider name");
        self.base_url = self.base_url.filter(|value| !value.is_empty());
        self.key_env = self.key_env.filter(|value| !value.is_empty());
        self.auth_file = self.auth_file.filter(|value| !value.as_os_str().is_empty());
        ensure!(
            self.key_env.as_deref().is_none_or(valid_env),
            "Invalid environment variable name"
        );
        let mut reasoning = BTreeMap::new();
        for (key, val) in self.reasoning {
            ensure!(
                matches!(key.as_str(), "thinking" | "reasoning_effort"),
                "Unknown reasoning field"
            );
            if let Some(table) = val.as_object() {
                ensure!(
                    key == "thinking" && table.keys().all(|key| key == "type"),
                    "Unknown reasoning field"
                );
            }
            reasoning.insert(key, toml::Value::try_from(val)?);
        }
        let auth_file = self.auth_file.map(|path| {
            if path.is_relative() {
                target.parent().unwrap_or(FilePath::new(".")).join(path)
            } else {
                path
            }
        });
        let settings = ProviderSettings {
            account: None,
            kind: Some(self.kind),
            base_url: self.base_url,
            key_env: self.key_env,
            auth_file,
            reasoning,
            models: self.models,
        };
        Ok(settings)
    }
}

fn model_json(model: &ProviderModelSettings) -> Value {
    let mut value = json!({"id": model.id});
    if let Some(enabled) = model.image_input {
        value["image_input"] = json!(enabled);
    }
    if let Some(policy) = model.reasoning_replay {
        value["reasoning_replay"] = json!(policy);
    }
    value
}

fn provider_table(provider: &ProviderSettings, previous: Option<&Item>) -> ApiResult<Item> {
    // Omitted optional scalars are removed from the target table. The save
    // handler refuses such removal if a lower layer would supply them again.
    let mut table = Table::new();
    table.insert(
        "kind",
        value(provider.kind.as_deref().unwrap_or("chat-completions")),
    );
    if let Some(base) = &provider.base_url {
        table.insert("base_url", value(base));
    }
    if let Some(key) = &provider.key_env {
        table.insert("key_env", value(key));
    }
    if let Some(path) = &provider.auth_file {
        table.insert("auth_file", value(path.to_string_lossy().into_owned()));
    }
    let fields = json!({
        "reasoning": provider.reasoning,
        "models": provider.models.iter().map(model_json).collect::<Vec<_>>(),
    });
    let fields = toml::Value::try_from(fields).map_err(|_| bad())?;
    let encoded = toml::to_string(&fields).map_err(|_| bad())?;
    let mut fields: DocumentMut = encoded.parse().map_err(|_| bad())?;
    table.insert(
        "reasoning",
        fields
            .remove("reasoning")
            .unwrap_or_else(|| value(toml_edit::InlineTable::new())),
    );
    table.insert(
        "models",
        fields
            .remove("models")
            .unwrap_or_else(|| value(toml_edit::Array::new())),
    );
    if let Some(previous) = previous.and_then(Item::as_table) {
        *table.decor_mut() = previous.decor().clone();
        if let Some(position) = previous.position() {
            table.set_position(position);
        }
        for (key, item) in table.iter_mut() {
            if let Some(old) = previous.get(key.get()) {
                match (old, item) {
                    (Item::Value(old), Item::Value(new)) => *new.decor_mut() = old.decor().clone(),
                    (Item::Table(old), Item::Table(new)) => *new.decor_mut() = old.decor().clone(),
                    _ => {}
                }
            }
        }
    }
    Ok(Item::Table(table))
}

async fn list(State(state): State<ProviderSetup>) -> ApiResult<Json<Value>> {
    let _guard = state.0.files.lock().map_err(|_| io_error())?;
    state.0.payload().map(Json)
}
async fn reload(State(state): State<ProviderSetup>) -> ApiResult<Response> {
    if let Some(host) = &state.0.host_config {
        let report = host.reload(None).await;
        let reply = {
            let _guard = state.0.files.lock().map_err(|_| io_error())?;
            state.0.payload()
        };
        let mut reply = reply.unwrap_or_else(|_| json!({}));
        reply["diff"] = serde_json::to_value(&report).map_err(|_| io_error())?;
        reply["applied"] = json!(report.applied);
        if !report.applied {
            reply["error"] = json!(report.summary());
        }
        let status = if report.applied {
            StatusCode::OK
        } else {
            StatusCode::BAD_REQUEST
        };
        return Ok((status, Json(reply)).into_response());
    }
    let _guard = state.0.files.lock().map_err(|_| io_error())?;
    let live = state.0.live.as_ref().ok_or_else(|| {
        ApiError(
            StatusCode::CONFLICT,
            "This host has no live provider configuration".into(),
        )
    })?;
    let _update = live.lock_update().map_err(|_| io_error())?;
    let config = live
        .startup_config()
        .reload()
        .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?;
    let candidate = live
        .prepare(config.clone())
        .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?;
    let document = state.0.document()?;
    let reply = state
        .0
        .payload_for(&config, &document, true, Some(candidate.main_model()))?;
    live.apply_prepared(candidate);
    Ok(Json(reply).into_response())
}
async fn save(
    State(state): State<ProviderSetup>,
    Path(name): Path<String>,
    input: std::result::Result<Json<ProviderInput>, JsonRejection>,
) -> ApiResult<Json<Value>> {
    let _host_update = match &state.0.host_config {
        Some(host) => Some(host.lock_update().await),
        None => None,
    };
    let provider = body(input)?
        .settings(&name, &state.0.target)
        .map_err(|_| bad())?;
    let _guard = state.0.files.lock().map_err(|_| io_error())?;
    let _update = state
        .0
        .live
        .as_ref()
        .map(|live| live.lock_update())
        .transpose()
        .map_err(|_| io_error())?;
    let settings = state.0.load()?.settings().map_err(|_| bad())?;
    validate_setup(&provider, &settings.web.provider_key_envs)?;
    let mut doc = state.0.document()?;
    let lower = state.0.lower()?;
    for (field, clearing) in [
        ("base_url", provider.base_url.is_none()),
        ("key_env", provider.key_env.is_none()),
        ("auth_file", provider.auth_file.is_none()),
    ] {
        if clearing && lower.value(&format!("providers.{name}.{field}")).is_some() {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!(
                    "{field} is inherited from defaults and can't be cleared here; set a value instead"
                ),
            ));
        }
    }
    let previous = doc.get("providers").and_then(|item| item.get(&name));
    let provider_item = provider_table(&provider, previous)?;
    table(&mut doc, "providers")?.insert(&name, provider_item);
    let candidate = state.0.candidate(&doc)?;
    let settings = candidate.settings().map_err(|_| bad())?;
    if settings.providers.get(&name).map(provider_json).as_ref() != Some(&provider_json(&provider))
    {
        return Err(conflict());
    }
    provider.validate(&name).map_err(|_| bad())?;
    state.0.write_apply(&doc, candidate).map(Json)
}
async fn remove(
    State(state): State<ProviderSetup>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let _host_update = match &state.0.host_config {
        Some(host) => Some(host.lock_update().await),
        None => None,
    };
    if !valid_name(&name) {
        return Err(bad());
    }
    let _guard = state.0.files.lock().map_err(|_| io_error())?;
    let _update = state
        .0
        .live
        .as_ref()
        .map(|live| live.lock_update())
        .transpose()
        .map_err(|_| io_error())?;
    let mut doc = state.0.document()?;
    if !target_provider(&doc, &name) {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "Provider is not in the target file".into(),
        ));
    }
    let lower = state.0.lower()?;
    let target_model = doc.get("model");
    let target_main_provider = target_model
        .and_then(|item| item.get("main"))
        .and_then(Item::as_str)
        .and_then(|model| {
            model
                .split_once('/')
                .map(|(provider, _)| provider)
                .or_else(|| {
                    target_model
                        .and_then(|item| item.get("provider"))
                        .and_then(Item::as_str)
                        .or_else(|| lower.value("model.provider").and_then(toml::Value::as_str))
                })
        });
    if target_main_provider.is_some_and(|provider| provider == name) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Provider is selected by model.main in the target file".into(),
        ));
    }
    if let Some(removed) = table(&mut doc, "providers")?.remove(&name)
        && let Some(removed) = removed.as_table()
    {
        // Header comments belong to the table's decoration in toml_edit.
        // Retain them even when removing the final table from the document.
        let mut trailing = doc.trailing().as_str().unwrap_or_default().to_owned();
        for decoration in [removed.decor().prefix(), removed.decor().suffix()]
            .into_iter()
            .flatten()
        {
            let text = decoration.as_str().unwrap_or_default();
            if text.contains('#') {
                trailing.push('\n');
                trailing.push_str(text);
                trailing.push('\n');
            }
        }
        doc.set_trailing(trailing);
    }
    let candidate = state.0.candidate(&doc)?;
    state.0.write_apply(&doc, candidate).map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MainInput {
    model: String,
}
async fn save_main(
    State(state): State<ProviderSetup>,
    input: std::result::Result<Json<MainInput>, JsonRejection>,
) -> ApiResult<Json<Value>> {
    let model = body(input)?.model;
    state.set_main_inner(&model).await.map(Json)
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TestInput {
    draft: Option<ProviderInput>,
}
async fn test(
    State(state): State<ProviderSetup>,
    Path(name): Path<String>,
    input: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult<Json<ProbeResult>> {
    if !valid_name(&name) {
        return Err(bad());
    }
    let bytes = input.map_err(|_| bad())?;
    let input: TestInput = if bytes.is_empty() {
        TestInput::default()
    } else {
        serde_json::from_slice(&bytes).map_err(|_| bad())?
    };
    let (provider, config) = {
        let _guard = state.0.files.lock().map_err(|_| io_error())?;
        let config = state.0.load()?;
        let settings = config.settings().map_err(|_| bad())?;
        let provider = match input.draft {
            Some(draft) => match draft.settings(&name, &state.0.target) {
                Ok(provider) => provider,
                Err(_) => return Ok(Json(ProbeResult::invalid("Invalid provider configuration"))),
            },
            None => settings
                .providers
                .get(&name)
                .cloned()
                .ok_or(ApiError(StatusCode::NOT_FOUND, "Unknown provider".into()))?,
        };
        validate_setup(&provider, &settings.web.provider_key_envs)?;
        if provider.validate(&name).is_err() {
            return Ok(Json(ProbeResult::invalid("Invalid provider configuration")));
        }
        (provider, config)
    };
    Ok(Json(probe(&name, &provider, &config).await))
}

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
enum ConfirmedHost {
    OpenAi,
    Anthropic,
    OpenRouter,
    OpenCode,
}
/// Exact kind/host pairs with documented non-generating probe endpoints.
fn confirmed_host(kind: &str, base: &reqwest::Url) -> Option<ConfirmedHost> {
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
fn model_url(base: &reqwest::Url, confirmed: Option<ConfirmedHost>) -> Result<reqwest::Url> {
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
fn auth_status(
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
fn codex_auth_file(provider: &ProviderSettings, config: &EffectiveConfig) -> Option<PathBuf> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::LoadOptions, provider::LiveProviderConfig};
    use std::sync::Arc;
    #[test]
    fn provider_save_preserves_invoking_environment_without_disclosing_it() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let file = workspace.path().join("config.toml");
        std::fs::write(
            &file,
            "[model]\nprovider='fixture'\nmain='fixture/model'\n[providers.fixture]\nkind='responses'\nbase_url='https://example.invalid/v1'\nkey_env='BP_WEB_SNAPSHOT_TEST_KEY'\n",
        )?;
        let secret = "synthetic-private-web-snapshot-key";
        let codex_home = workspace.path().join("client-codex");
        let launched = EffectiveConfig::load(LoadOptions {
            workspace: workspace.path().to_owned(),
            home: None,
            custom_file: Some(file),
            env: vec![
                ("BP_WEB_SNAPSHOT_TEST_KEY".into(), secret.into()),
                (
                    "CODEX_HOME".into(),
                    codex_home.to_string_lossy().into_owned(),
                ),
            ],
            cli: vec![],
        })?;
        assert!(launched.load_options().env.is_empty());
        let live = Arc::new(LiveProviderConfig::new(launched.clone())?);
        let setup = ProviderSetup::new(launched.load_options(), &launched)?.with_live(live.clone());
        let mut document = setup
            .0
            .document()
            .map_err(|error| anyhow::anyhow!(error.1))?;
        document["providers"]["fixture"]["base_url"] = value("https://example.invalid/v2");
        let candidate = setup
            .0
            .candidate(&document)
            .map_err(|error| anyhow::anyhow!(error.1))?;
        assert_eq!(
            candidate.environment("BP_WEB_SNAPSHOT_TEST_KEY").as_deref(),
            Some(secret)
        );
        assert!(!format!("{candidate:?}").contains(secret));
        let reply = setup
            .0
            .write_apply(&document, candidate)
            .map_err(|error| anyhow::anyhow!(error.1))?;
        let provider = reply["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|provider| provider["name"] == "fixture")
            .unwrap();
        assert_eq!(provider["key_present"], true);
        assert!(!reply.to_string().contains(secret));
        let active = live.snapshot();
        assert_eq!(
            active
                .effective
                .environment("BP_WEB_SNAPSHOT_TEST_KEY")
                .as_deref(),
            Some(secret)
        );
        assert_eq!(
            active.settings.providers["fixture"].base_url.as_deref(),
            Some("https://example.invalid/v2")
        );
        assert_eq!(
            codex_auth_file(&active.settings.providers["fixture"], &active.effective),
            Some(codex_home.join("auth.json"))
        );
        Ok(())
    }

    #[test]
    fn loopback_url_accepts_only_machine_local_hosts() {
        for host in [
            "localhost",
            "nested.localhost",
            "a.b.localhost",
            "127.0.0.1",
            "127.255.255.254",
            "[::1]",
        ] {
            assert!(loopback_url(
                &reqwest::Url::parse(&format!("http://{host}:8000/v1")).unwrap()
            ));
        }
        for host in [
            "localhost.example.org",
            "notlocalhost",
            "128.0.0.1",
            "192.168.1.2",
            "[::2]",
        ] {
            assert!(!loopback_url(
                &reqwest::Url::parse(&format!("http://{host}:8000/v1")).unwrap()
            ));
        }
    }

    #[test]
    fn opencode_models_mapping_is_documented_but_not_an_auth_check() {
        for kind in KINDS
            .iter()
            .copied()
            .filter(|kind| !matches!(*kind, "codex" | "scripted"))
        {
            for (base, expected) in [
                (
                    "https://opencode.ai/zen/go/v1",
                    "https://opencode.ai/inference/go/v1/models",
                ),
                (
                    "https://opencode.ai/zen/v1",
                    "https://opencode.ai/inference/v1/models",
                ),
                (
                    "https://opencode.ai/zen/go/v1/messages",
                    "https://opencode.ai/inference/go/v1/models",
                ),
                (
                    "https://opencode.ai:444/zen/go/v1",
                    "https://opencode.ai/inference/go/v1/models",
                ),
            ] {
                let base = reqwest::Url::parse(base).unwrap();
                let confirmed = confirmed_host(kind, &base);
                assert_eq!(confirmed, Some(ConfirmedHost::OpenCode));
                assert_eq!(model_url(&base, confirmed).unwrap().as_str(), expected);
            }
        }
        for base in [
            "https://opencode.ai.example.org/zen/go/v1",
            "http://opencode.ai/zen/go/v1",
        ] {
            assert_eq!(
                confirmed_host("chat-completions", &reqwest::Url::parse(base).unwrap()),
                None
            );
        }
        for code in 100..=599 {
            let status = reqwest::StatusCode::from_u16(code).unwrap();
            let expected = if matches!(code, 401 | 403) {
                ("failed", "endpoint rejected authentication")
            } else {
                (
                    "unchecked",
                    "models list documented; auth not confirmed for this endpoint",
                )
            };
            assert_eq!(auth_status(Some(ConfirmedHost::OpenCode), status), expected);
        }
        for confirmed in [
            ConfirmedHost::OpenAi,
            ConfirmedHost::Anthropic,
            ConfirmedHost::OpenRouter,
        ] {
            assert_eq!(
                auth_status(Some(confirmed), reqwest::StatusCode::OK).0,
                "ok"
            );
        }
        assert_eq!(auth_status(None, reqwest::StatusCode::OK).0, "unchecked");
    }

    #[test]
    fn confirmed_host_table_is_exact() {
        for kind in [
            "responses",
            "chat-completions",
            "decisions",
            "openai-decisions",
        ] {
            assert_eq!(
                confirmed_host(
                    kind,
                    &reqwest::Url::parse("https://api.openai.com").unwrap()
                ),
                Some(ConfirmedHost::OpenAi)
            );
            assert_eq!(
                model_url(
                    &reqwest::Url::parse("https://api.openai.com").unwrap(),
                    Some(ConfirmedHost::OpenAi)
                )
                .unwrap()
                .as_str(),
                "https://api.openai.com/v1/models"
            );
        }
        assert_eq!(
            confirmed_host(
                "anthropic-messages",
                &reqwest::Url::parse("https://api.anthropic.com/v1").unwrap()
            ),
            Some(ConfirmedHost::Anthropic)
        );
        assert_eq!(
            confirmed_host(
                "openrouter-decisions",
                &reqwest::Url::parse("https://openrouter.ai/api/alpha/decisions").unwrap()
            ),
            Some(ConfirmedHost::OpenRouter)
        );
        for base in [
            "https://api.openai.com.example.org/v1",
            "http://api.openai.com/v1",
            "https://api.openai.com:444/v1",
            "http://127.0.0.1:8000/v1",
        ] {
            assert_eq!(
                confirmed_host("responses", &reqwest::Url::parse(base).unwrap()),
                None
            );
        }
        assert_eq!(
            confirmed_host(
                "anthropic-messages",
                &reqwest::Url::parse("https://api.openai.com/v1").unwrap()
            ),
            None
        );
        assert_eq!(
            model_url(
                &reqwest::Url::parse("https://api.anthropic.com/v1/").unwrap(),
                Some(ConfirmedHost::Anthropic)
            )
            .unwrap()
            .as_str(),
            "https://api.anthropic.com/v1/models?limit=1000"
        );
    }
}
