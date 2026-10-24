//! HTTP Responses adapter. Provider wire data and credentials stay out of core.
mod chat;
pub(crate) use chat::is_loopback_url;
mod continuation;
mod decisions;
mod endpoints;
mod extension;
mod http_dump;
mod http_error;
mod live_provider;
mod messages;
mod registry;
pub(crate) mod replay_state;
mod responses_body;
mod retry;
mod scripted;
mod stream;
mod tool_wire;
use anyhow::{Context, Result, anyhow, ensure};
use async_trait::async_trait;
use bubblepaw_core::{
    CacheMode, CancellationToken, Capabilities, ModelInfo, ModelRequest, ModelResponse, Provider,
    ProviderError, ProviderErrorKind, ReasoningEffort, Role, TextCallback, ToolCall, ToolSurface,
    Usage,
};
pub use chat::ChatCompletionsProvider;
pub(crate) use decisions::with_decision_usage;
pub use decisions::{
    LlmDecisions, OpenAiDecisions, OpenCodeDecisions, OpenRouterDecisions, decision_error_usage,
};
pub use endpoints::{ProviderEndpoints, ProviderKeys};
pub use extension::{
    EXTENSION_ENV_KEYS, ExtensionConfig, ExtensionProvider, load_extensions,
    valid_env_name as valid_extension_env_name,
};
pub use live_provider::{
    LiveProviderConfig, PreparedProviderConfig, ProviderGeneration, build_provider_registry,
};
pub use messages::MessagesProvider;
pub use registry::ProviderRegistry;
pub use retry::RetryPolicy;
pub(crate) use retry::with_retry;
pub use scripted::{ScriptedProvider, ScriptedStep};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};
use stream::{SseReader, TextBatch};

enum Credentials {
    ApiKey(String),
    CodexAuth(PathBuf),
}
pub struct ResponsesProvider {
    client: reqwest::Client,
    credentials: Credentials,
    base_url: String,
    models: Vec<String>,
    reasoning_effort: Option<String>,
    opencode_route: bool,
}
impl ResponsesProvider {
    pub fn api_key(key: impl Into<String>, base_url: impl Into<String>) -> Result<Self> {
        let key = key.into();
        ensure!(!key.is_empty(), "API key is empty");
        Self::new(Credentials::ApiKey(key), base_url.into())
    }
    pub fn codex_auth(path: impl AsRef<Path>, base_url: impl Into<String>) -> Result<Self> {
        read_codex(path.as_ref())?;
        Self::new(
            Credentials::CodexAuth(path.as_ref().to_owned()),
            base_url.into(),
        )
    }
    fn new(credentials: Credentials, base_url: String) -> Result<Self> {
        let url = reqwest::Url::parse(&base_url).context("invalid provider base URL")?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "provider URL must be HTTP(S) without credentials/query/fragment"
        );
        Ok(Self {
            client: stream::client()?,
            credentials,
            base_url: base_url.trim_end_matches('/').into(),
            models: Vec::new(),
            reasoning_effort: None,
            opencode_route: false,
        })
    }
    pub fn with_models(mut self, models: Vec<String>) -> Self {
        self.models = models;
        self
    }
    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self
    }
    /// Marks this provider as an OpenCode Go route, which requires the
    /// `x-opencode-session` header. Set only from OpenCode provider aliases.
    pub fn with_opencode_route(mut self) -> Self {
        self.opencode_route = true;
        self
    }
    async fn attempt(
        &self,
        body: &Value,
        on_text: &TextCallback,
        cancellation: &CancellationToken,
        key: &str,
        retried_auth: &mut bool,
        session: Option<&str>,
    ) -> Result<(ModelResponse, bool)> {
        let model = body["model"].as_str().unwrap_or_default();
        let mut meter = PhysicalRequest::new("responses", model);
        let identity = bubblepaw_core::RequestUsage::new("codex", model);
        let mut quota = Vec::new();
        let result: Result<(ModelResponse, bool)> = async {
        let (mut bearer, mut account) =
            match &self.credentials {
                Credentials::ApiKey(key) => (key.clone(), None),
                Credentials::CodexAuth(path) => read_codex(path)
                    .map(|(a, b)| (a, Some(b)))
                    .map_err(|error| classify(ProviderErrorKind::Auth, error))?,
            };
        let response = loop {
            let mut builder = self
                .client
                .post(format!("{}/responses", self.base_url))
                .bearer_auth(&bearer)
                .header("Idempotency-Key", key)
                .header("User-Agent", "bubblepaw/lean-rust");
            if let Some(session) = session {
                builder = builder.header("x-opencode-session", session);
            }
            if let Some(account) = &account {
                builder = builder
                    .header("ChatGPT-Account-ID", account)
                    .header("originator", "bubblepaw")
                    .header("OpenAI-Beta", "responses=experimental");
            }
            let dump = http_dump::HttpDump::begin(body, "responses");
            let sent = tokio::select! {result=tokio::time::timeout(stream::IDLE_TIMEOUT,builder.json(body).send())=>result,_=cancellation.cancelled()=>return Err(cancelled())};
            let mut response = sent
                .map_err(|_| transient(anyhow!("provider response was idle too long")))?
                .map_err(|error| {
                    transient(anyhow!(
                        "provider transport failed ({})",
                        if error.is_timeout() {
                            "timeout"
                        } else if error.is_connect() {
                            "connect"
                        } else {
                            "request"
                        }
                    ))
                })?;
            if let Some(dump) = dump {
                dump.attach(&mut response);
            }
            if response.status().as_u16() == 401
                && let Credentials::CodexAuth(path) = &self.credentials
            {
                let (current, current_account) = read_codex(path)?;
                if !*retried_auth && current != bearer {
                    bearer = current;
                    account = Some(current_account);
                    *retried_auth = true;
                    meter.finish(None, true);
                    meter = PhysicalRequest::new("responses", model);
                    if http_dump::enabled(&response) {
                        let _ = http_error::read_body(response, cancellation).await;
                    }
                    continue;
                }
                let bytes = http_error::read_body(response, cancellation).await?;
                let diagnostic = http_error::diagnostic(
                    "Responses",
                    401,
                    &bytes,
                    &[&bearer, account.as_deref().unwrap_or_default()],
                );
                return Err(classify(
                    ProviderErrorKind::Auth,
                    anyhow!(
                        "{diagnostic}; Codex CLI owns token refresh; run codex (or codex login) to refresh credentials in {}",
                        path.display()
                    ),
                ));
            }
            break response;
        };
        if matches!(self.credentials, Credentials::CodexAuth(_)) {
            quota = crate::quota_sources::parse_codex_headers(&identity.provider, &identity.account, response.headers(), chrono::Utc::now());
        }
        let status = response.status();
        if !status.is_success() {
            let retry_after = http_error::retry_after(response.headers());
            let code = status.as_u16();
            let bytes = http_error::read_body(response, cancellation).await?;
            if let Some(reason) = http_error::terminal_reason(&bytes) {
                return Err(terminal_quota_failure(reason, http_error::diagnostic("Responses", code, &bytes, &[&bearer])));
            }
            let kind = http_error::status_kind(
                code,
                retry_after,
                is_context_overflow(&String::from_utf8_lossy(&bytes)),
            );
            return Err(attach_retry_after(
                classify(
                    kind,
                    anyhow!(http_error::diagnostic(
                        "Responses",
                        code,
                        &bytes,
                        &[&bearer, account.as_deref().unwrap_or_default()],
                    )),
                ),
                code,
                retry_after,
            ));
        }
        let mut reader = SseReader::new(response);
        let mut batch = TextBatch::new(on_text.clone());
        let mut text = String::new();
        let mut items = BTreeMap::<u64, Value>::new();
        let mut completed = None;
        let mut published = false;
        let outcome = stream::pump(
            &mut reader,
            &mut batch,
            cancellation,
            "provider stream ended without response.completed",
            |data, batch| {
                if data == "[DONE]" {
                    return Ok(false);
                }
                let event: Value = serde_json::from_str(data).map_err(|_| {
                    stream::failed(ProviderErrorKind::Fatal, "provider SSE JSON is invalid")
                })?;
                match event["type"].as_str().unwrap_or("") {
                    "response.output_text.delta" => {
                        let delta = event["delta"].as_str().unwrap_or("");
                        text.push_str(delta);
                        batch.push(delta);
                        published |= !delta.is_empty();
                    }
                    "response.output_item.done" => {
                        batch.flush();
                        items.insert(
                            event["output_index"].as_u64().unwrap_or(items.len() as u64),
                            event["item"].clone(),
                        );
                    }
                    "response.completed" => {
                        completed = Some(event["response"].clone());
                        return Ok(true);
                    }
                    // A length stop is a partial reply; generate continues it.
                    "response.incomplete" if is_length_stop(&event["response"]) => {
                        completed = Some(event["response"].clone());
                        return Ok(true);
                    }
                    "response.failed" | "response.incomplete" | "error" => {
                        if let Some(reason) = http_error::terminal_reason(event.to_string().as_bytes()) {
                            return Err(stream::failed(ProviderErrorKind::Fatal, &format!("{reason}: provider generation rejected")));
                        }
                        let kind = if is_context_overflow(&event.to_string()) {
                            ProviderErrorKind::Context
                        } else {
                            let error = if event["type"] == "response.failed" {
                                &event["response"]["error"]
                            } else {
                                &event
                            };
                            match error["code"]
                                .as_str()
                                .or_else(|| error["error"]["code"].as_str())
                            {
                                Some("server_error") => ProviderErrorKind::Transient,
                                Some("rate_limit_exceeded") => {
                                    ProviderErrorKind::RateLimit { resets_at: None }
                                }
                                _ => ProviderErrorKind::Fatal,
                            }
                        };
                        return Err(stream::failed(
                            kind,
                            "provider generation failed or incomplete",
                        ));
                    }
                    _ => {}
                }
                Ok(false)
            },
        )
        .await;
        match outcome {
            Ok(()) => {}
            // A published failure is fatal for the registry too: it never retries
            // once text reached the listener, so the kind no longer depends on it.
            Err(error) => return Err(error.into_error()),
        }
        let response = completed
            .ok_or_else(|| transient(anyhow!("provider stream ended without response.completed")))?;
        let cut = is_length_stop(&response);
        let usage = response
            .get("usage")
            .filter(|v| v.is_object())
            .map(|v| Usage {
                input_tokens: v["input_tokens"].as_u64().unwrap_or(0),
                output_tokens: v["output_tokens"].as_u64().unwrap_or(0),
                cache_read_tokens: v["input_tokens_details"]["cached_tokens"]
                    .as_u64()
                    .unwrap_or(0),
                cache_creation_tokens: v["input_tokens_details"]["cache_creation_tokens"]
                    .as_u64()
                    .or_else(|| v["cache_creation_input_tokens"].as_u64())
                    .unwrap_or(0),
                reasoning_tokens: v["output_tokens_details"]["reasoning_tokens"].as_u64(),
            });
        let malformed_call = |message: &str| {
            let error = anyhow!(message.to_owned());
            let error = match usage.as_ref() {
                Some(usage) => decisions::metered_model_error(error, Some(usage.clone())),
                None => error,
            };
            classify(ProviderErrorKind::Fatal, error)
        };
        let mut final_items: Vec<Value> = response["output"]
            .as_array()
            .filter(|v| !v.is_empty())
            .cloned()
            .unwrap_or_else(|| items.into_values().collect());
        if cut {
            // A call the length stop cut is dropped from the reply, so its item
            // must not reach the continuation request without an output.
            final_items.retain(|item| {
                item["type"] != "function_call"
                    || item["arguments"]
                        .as_str()
                        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                        .is_some_and(|arguments| arguments.is_object())
            });
        }
        let mut calls = Vec::new();
        let mut seen = HashSet::new();
        let mut final_text = String::new();
        let mut provider_state = Some(json!({"adapter":"responses","items":final_items}));
        for item in provider_state.as_ref().unwrap()["items"]
            .as_array()
            .unwrap()
        {
            match item["type"].as_str().unwrap_or("") {
                "function_call" => {
                    let id = item["call_id"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| malformed_call("function call missing call_id"))?;
                    let name = item["name"]
                        .as_str()
                        .filter(|name| !name.is_empty())
                        .ok_or_else(|| malformed_call("function call missing name"))?;
                    if !seen.insert(id.to_owned()) {
                        return Err(malformed_call("duplicate provider call_id"));
                    }
                    // Declarations on a tools-off request are inactive: tool_choice is none.
                    if !tool_wire::accepts_call(name, body["tool_choice"] != "none" && body["tools"]
                        .as_array()
                        .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == name)))
                    {
                        return Err(malformed_call("provider returned an unavailable tool"));
                    }
                    let arguments = match serde_json::from_str::<Value>(
                        item["arguments"]
                            .as_str()
                            .ok_or_else(|| malformed_call("invalid function call arguments"))?,
                    ) {
                        Ok(arguments) => arguments,
                        // A length stop can cut a call's arguments; that call is dropped.
                        Err(_) if cut => continue,
                        Err(_) => {
                            return Err(malformed_call("invalid function call arguments"));
                        }
                    };
                    if !arguments.is_object() {
                        return Err(malformed_call("function call arguments must be an object"));
                    }
                    calls.push(ToolCall {
                        id: id.into(),
                        name: name.into(),
                        arguments,
                    });
                }
                "message" => {
                    if let Some(content) = item["content"].as_array() {
                        for part in content {
                            if part["type"] == "output_text" {
                                final_text.push_str(part["text"].as_str().unwrap_or(""));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if final_text.is_empty() {
            final_text = text.clone();
            if !final_text.is_empty() {
                provider_state.as_mut().unwrap()["items"].as_array_mut().unwrap().push(json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":final_text}]}));
            }
        }
        if !published && !final_text.is_empty() {
            on_text(&final_text);
        }
        if published && final_text != text {
            return Err(classify(
                ProviderErrorKind::Fatal,
                anyhow!("provider final text conflicts with streamed text"),
            ));
        }
        Ok((
            ModelResponse {
                text: final_text,
                tool_calls: calls,
                last_request_usage: Some(bubblepaw_core::LastRequestUsage {
                    input_tokens: response["usage"]["input_tokens"].as_u64(),
                    output_tokens: response["usage"]["output_tokens"].as_u64(),
                }),
                usage,
                provider_state,
                quota: quota.clone(),
            },
            cut,
        ))
        }.await;
        let result = result.map_err(|error| crate::quota_sources::with_error_quota(error, quota));
        let usage = match &result {
            Ok((response, _)) => response.usage.as_ref(),
            Err(error) => ProviderError::of(error).and_then(|error| error.usage.as_ref()),
        };
        meter.finish(usage, result.is_err());
        result
    }
}

/// Whether a Responses object stopped on the output limit (a partial reply).
fn is_length_stop(response: &Value) -> bool {
    response["status"] == "incomplete"
        && response["incomplete_details"]["reason"] == "max_output_tokens"
}

/// Joins the rounds of one reply. A single round is returned unchanged. For
/// several rounds, the stored items keep reasoning from every round and carry
/// one assistant message with the joined text, so replay does not repeat text.
fn join_rounds(mut rounds: Vec<ModelResponse>) -> ModelResponse {
    if rounds.len() == 1 {
        return rounds.remove(0);
    }
    let mut text = String::new();
    let mut total = Usage::default();
    let mut any_usage = false;
    let mut items = Vec::new();
    let mut tool_calls = Vec::new();
    let mut last_request_usage = None;
    let mut quota = Vec::new();
    for round in rounds {
        text.push_str(&round.text);
        if let Some(part) = &round.usage {
            total.add(part);
            any_usage = true;
        }
        if let Some(list) = round
            .provider_state
            .as_ref()
            .and_then(|state| state["items"].as_array())
        {
            // Preserve native ordering, IDs and opaque metadata across ALL
            // rounds. Slim references remove the canonical text duplication.
            items.extend(list.iter().cloned());
        }
        tool_calls = round.tool_calls;
        last_request_usage = round.last_request_usage;
        quota.extend(round.quota);
    }
    ModelResponse {
        text,
        tool_calls,
        usage: any_usage.then_some(total),
        last_request_usage,
        quota,
        provider_state: Some(json!({"adapter":"responses","items":items})),
    }
}
#[async_trait]
impl Provider for ResponsesProvider {
    fn models(&self) -> Vec<ModelInfo> {
        self.models
            .iter()
            .map(|id| ModelInfo {
                id: id.clone(),
                name: id.clone(),
            })
            .collect()
    }
    fn capabilities(&self, _model: &str) -> Capabilities {
        // No model window table is built in; the summarizer reads Codex metadata.
        Capabilities {
            context_window: None,
            cache: CacheMode::Prefix,
            tool_surface: ToolSurface::Direct,
            stateful: false,
            image_input: true,
        }
    }
    async fn generate_decision(
        &self,
        request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        self.generate_body(request, true, on_text, cancellation)
            .await
    }
    async fn generate(
        &self,
        request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        self.generate_body(request, false, on_text, cancellation)
            .await
    }
    fn wire_shape(&self, request: &ModelRequest) -> Option<bubblepaw_core::WireShape> {
        // `body` is pure; `generate_body` posts this body for main-loop rounds.
        let body = self.body(request).ok()?;
        let mut shape = bubblepaw_core::WireShape::from_body(
            &body,
            "input",
            bubblepaw_core::DEFAULT_CACHE_BLOCK_TOKENS,
        );
        shape.model = request.model.clone();
        Some(shape)
    }
}
impl ResponsesProvider {
    /// Sends the reply as one or more requests. A length stop is continued; the
    /// rounds are joined into one response. `decision` keeps the request's own
    /// reasoning effort, as decision calls do.
    async fn generate_body(
        &self,
        request: ModelRequest,
        decision: bool,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let (on_text, published) = track_text_publication(on_text);
        let mut known_usage = None::<Usage>;
        let mut known_quota = Vec::new();
        let result = async {
            let session = self.opencode_route.then(|| opencode_session(&request));
            let mut request = request;
            let mut rounds = Vec::new();
            loop {
                let mut body = self
                    .body(&request)
                    .map_err(|error| classify(ProviderErrorKind::Fatal, error))?;
                if decision {
                    body["reasoning"] =
                        json!({"effort":request.reasoning_effort.unwrap_or_default().as_str()});
                }
                let (mut response, cut) = self
                    .send_round(
                        &body,
                        &on_text,
                        &cancellation,
                        &published,
                        session.as_deref(),
                    )
                    .await?;
                if let Some(usage) = &response.usage {
                    known_usage.get_or_insert_with(Usage::default).add(usage);
                }
                known_quota.extend(response.quota.iter().cloned());
                if let Some(state) = response.provider_state.take() {
                    response.provider_state = Some(bubblepaw_core::with_provider_state_origin(
                        state,
                        "responses",
                        Some(&request.model),
                        "native",
                    )?);
                }
                let has_calls = !response.tool_calls.is_empty();
                let continuations = rounds.len();
                // The continuation carries this round, not just its visible text.
                let next = continuation::should_continue(cut, has_calls, rounds.len())
                    .then(|| continuation::continue_response(&request, &response));
                rounds.push(response);
                if let Some(next) = next {
                    request = next;
                    continue;
                }
                if cut {
                    continuation::warn_incomplete("responses", &request.model, continuations);
                }
                let mut joined = join_rounds(rounds);
                if let Some(state) = joined.provider_state.take() {
                    joined.provider_state = Some(bubblepaw_core::with_provider_state_origin(
                        replay_state::slim(state, &joined.text, &joined.tool_calls)?,
                        "responses",
                        Some(&request.model),
                        "native",
                    )?);
                }
                return Ok(joined);
            }
        }
        .await;
        result.map_err(|error| {
            mark_published(
                retain_prior_quota(retain_prior_usage(error, known_usage.as_ref()), known_quota),
                published.load(Ordering::Acquire),
            )
        })
    }

    /// One request. The registry owns transport retries, so this sends a single
    /// attempt and returns the reply and whether it stopped on the output limit.
    async fn send_round(
        &self,
        body: &Value,
        on_text: &TextCallback,
        cancellation: &CancellationToken,
        published: &AtomicBool,
        session: Option<&str>,
    ) -> Result<(ModelResponse, bool)> {
        let key = uuid::Uuid::new_v4().to_string();
        let mut retried_auth = false;
        self.attempt(
            body,
            on_text,
            cancellation,
            &key,
            &mut retried_auth,
            session,
        )
        .await
        .map_err(|error| mark_published(error, published.load(Ordering::Acquire)))
    }
}

/// One physical transport request. A dropped request is recorded as failed;
/// known usage is filled before provider continuation totals are accumulated.
pub(crate) struct PhysicalRequest(Option<bubblepaw_core::RequestUsage>);
impl PhysicalRequest {
    pub(crate) fn new(provider: &str, model: &str) -> Self {
        Self(Some(bubblepaw_core::RequestUsage::new(provider, model)))
    }
    pub(crate) fn name_model(&mut self, model: &str) {
        if let Some(record) = self.0.as_mut() {
            record.model = model.to_owned();
        }
    }
    pub(crate) fn finish(&mut self, usage: Option<&Usage>, failed: bool) {
        if let Some(mut record) = self.0.take() {
            record.usage = usage.cloned().unwrap_or_default();
            record.failed = failed;
            bubblepaw_core::record_request_usage(record);
        }
    }
    pub(crate) fn finish_result(&mut self, result: &Result<ModelResponse>) {
        let usage = match result {
            Ok(response) => response.usage.as_ref(),
            Err(error) => ProviderError::of(error).and_then(|error| error.usage.as_ref()),
        };
        self.finish(usage, result.is_err());
    }
}
impl Drop for PhysicalRequest {
    fn drop(&mut self) {
        if let Some(mut record) = self.0.take() {
            record.failed = true;
            bubblepaw_core::record_request_usage(record);
        }
    }
}

/// Publication follows listener delivery, including prior continuation rounds.
fn track_text_publication(on_text: TextCallback) -> (TextCallback, Arc<AtomicBool>) {
    let published = Arc::new(AtomicBool::new(false));
    let signal = published.clone();
    let listener: TextCallback = Arc::new(move |text| {
        if !text.is_empty() {
            signal.store(true, Ordering::Release);
        }
        on_text(text);
    });
    (listener, published)
}

/// Keep accepted physical continuation usage when a later response fails.
fn retain_prior_usage(error: anyhow::Error, prior: Option<&Usage>) -> anyhow::Error {
    let Some(prior) = prior else {
        return error;
    };
    let typed = ProviderError::of(&error)
        .cloned()
        .unwrap_or_else(|| ProviderError::new(ProviderErrorKind::Fatal, error.to_string()));
    let mut total = prior.clone();
    if let Some(usage) = &typed.usage {
        total.add(usage);
    }
    error.context(typed.with_usage(total))
}

/// Preserve the error chain so usage and provider Retry-After remain available.
fn mark_published(error: anyhow::Error, published: bool) -> anyhow::Error {
    if !published {
        return error;
    }
    let typed = ProviderError::of(&error)
        .cloned()
        .unwrap_or_else(|| ProviderError::new(ProviderErrorKind::Fatal, error.to_string()));
    if typed.published {
        error
    } else {
        error.context(typed.with_published(true))
    }
}
/// Parse Retry-After delta-seconds or an HTTP date. Past dates mean no delay.
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value.parse::<u64>().ok().map(Duration::from_secs);
    }
    httpdate::parse_http_date(value).ok().map(|deadline| {
        deadline
            .duration_since(SystemTime::now())
            .unwrap_or_default()
    })
}

/// Remaining server-requested delay, preserved through the provider error chain.
/// Adapters record it; the registry's retry policy decides what to do with it.
pub fn provider_retry_after(error: &anyhow::Error) -> Option<Duration> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<RetryAfterError>())
        .map(|error| error.deadline.saturating_duration_since(Instant::now()))
}

/// Keep all observations, with a stable boundary around the current rejection.
/// Earlier retry/continuation observations cannot name its rejecting window.
fn retain_prior_quota(
    error: anyhow::Error,
    quota: Vec<bubblepaw_core::QuotaReading>,
) -> anyhow::Error {
    let current_count = if current_rejection_quota_count(&error).is_none() {
        ProviderError::of(&error)
            .filter(|typed| matches!(typed.kind, ProviderErrorKind::RateLimit { .. }))
            .map(|typed| typed.quota.len())
    } else {
        None
    };
    let error = crate::quota_sources::with_error_quota(error, quota);
    match current_count {
        Some(count) => {
            let message = error.to_string();
            error.context(RejectionQuotaBoundary { count, message })
        }
        None => error,
    }
}

pub(crate) fn current_rejection_quota_count(error: &anyhow::Error) -> Option<usize> {
    error
        .downcast_ref::<RejectionQuotaBoundary>()
        .map(|boundary| boundary.count)
}

#[derive(Debug)]
struct RejectionQuotaBoundary {
    count: usize,
    message: String,
}
impl std::fmt::Display for RejectionQuotaBoundary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[derive(Debug)]
struct RetryAfterError {
    deadline: Instant,
    source: anyhow::Error,
}
impl std::fmt::Display for RetryAfterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.source, formatter)
    }
}
impl std::error::Error for RetryAfterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}
fn retryable_http_status(code: u16) -> bool {
    matches!(code, 408 | 409 | 429 | 500..=599)
}
fn attach_retry_after(error: anyhow::Error, code: u16, delay: Option<Duration>) -> anyhow::Error {
    if retryable_http_status(code)
        && let Some(deadline) = delay.and_then(|delay| Instant::now().checked_add(delay))
    {
        RetryAfterError {
            deadline,
            source: error,
        }
        .into()
    } else {
        error
    }
}
/// Attach a kind, keeping the message exactly as before.
fn classify(kind: ProviderErrorKind, error: anyhow::Error) -> anyhow::Error {
    if ProviderError::of(&error).is_some() {
        return error;
    }
    ProviderError::new(kind, error.to_string()).into()
}
fn parse_reasoning_effort(effort: &str) -> Result<ReasoningEffort> {
    serde_json::from_value(json!(effort)).map_err(|_| {
        anyhow!("Unsupported reasoning effort; expected low, medium, high, xhigh or max")
    })
}
fn transient(error: anyhow::Error) -> anyhow::Error {
    classify(ProviderErrorKind::Transient, error)
}
pub(crate) fn cancelled() -> anyhow::Error {
    ProviderError::new(ProviderErrorKind::Transient, "cancelled").into()
}

/// Hosts must not retry or silently route a terminal account billing failure.
pub fn is_terminal_quota_error(error: &ProviderError) -> bool {
    matches!(error.kind, ProviderErrorKind::Fatal)
        && (error.message.starts_with("billing or spend limit reached")
            || error
                .message
                .starts_with("provider marked request non-retryable"))
}
fn terminal_quota_failure(reason: &str, diagnostic: String) -> anyhow::Error {
    ProviderError::new(ProviderErrorKind::Fatal, format!("{reason}: {diagnostic}")).into()
}

/// Session id for the `x-opencode-session` header, chosen the same way for
/// every wire that sends it: the request's session, else its first message's
/// session, else its first message id, else a fresh UUID.
pub(crate) fn opencode_session(request: &ModelRequest) -> String {
    (!request.session_id.is_empty())
        .then_some(request.session_id.as_str())
        .or_else(|| {
            request
                .messages
                .iter()
                .find(|message| !message.session_id.is_empty())
                .map(|message| message.session_id.as_str())
        })
        .or_else(|| {
            request
                .messages
                .iter()
                .find(|message| !message.id.is_empty())
                .map(|message| message.id.as_str())
        })
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}
fn is_context_overflow(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("context_length_exceeded")
        || lower.contains("maximum context length")
        || lower.contains("context window")
}
fn read_codex(path: &Path) -> Result<(String, String)> {
    let auth_error = |message: &str| {
        classify(
            ProviderErrorKind::Auth,
            anyhow!("{message}: {}", path.display()),
        )
    };
    let bytes = std::fs::read(path).map_err(|_| auth_error("cannot read Codex auth file"))?;
    let auth: Value = serde_json::from_slice(&bytes)
        .map_err(|_| auth_error("invalid JSON in Codex auth file"))?;
    let field = |name| {
        auth["tokens"][name]
            .as_str()
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| auth_error("Codex auth file lacks access_token/account_id"))
    };
    Ok((field("access_token")?, field("account_id")?))
}
