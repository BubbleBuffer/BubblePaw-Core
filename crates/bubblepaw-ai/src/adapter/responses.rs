//! The Responses adapter: construction and one HTTP attempt.
use super::*;

pub(super) enum Credentials {
    ApiKey(String),
    CodexAuth(PathBuf),
}
pub struct ResponsesProvider {
    pub(in crate::adapter) client: stream::Client,
    pub(super) credentials: Credentials,
    pub(super) base_url: String,
    pub(super) models: Vec<String>,
    pub(super) reasoning_effort: Option<String>,
    pub(super) opencode_route: bool,
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
    pub(super) async fn attempt(
        &self,
        body: &Value,
        on_text: &StreamCallback,
        cancellation: &CancellationToken,
        key: &str,
        retried_auth: &mut bool,
        route_key: Option<&str>,
    ) -> Result<(ModelResponse, bool)> {
        let model = body["model"].as_str().unwrap_or_default();
        let mut meter = PhysicalRequest::new("responses", model);
        let usage_account = crate::current_account().unwrap_or_else(|| "codex".to_owned());
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
            if let Some(route_key) = route_key {
                builder = builder.header("x-opencode-session", route_key);
            }
            if let Some(account) = &account {
                builder = codex_cache_headers(builder, body)
                    .header("ChatGPT-Account-ID", account)
                    .header("originator", "bubblepaw")
                    .header("OpenAI-Beta", "responses=experimental");
            }
            let dump = http_dump::HttpDump::begin(body, "responses");
            let sent = tokio::select! {result=tokio::time::timeout(stream::IDLE_TIMEOUT,builder.json(body).send())=>result,_=cancellation.cancelled()=>return Err(cancelled())};
            let mut response = sent
                .map_err(|_| transient(anyhow!("provider response was idle too long")))?
                .map_err(|error| {
                    self.client.transport_failed(format!(
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
            quota = crate::adapter::quota_headers::parse_codex_headers("codex", &usage_account, response.headers(), chrono::Utc::now());
        }
        let status = response.status();
        if !status.is_success() {
            let retry_after = http_error::retry_after(response.headers());
            let code = status.as_u16();
            let bytes = http_error::read_body(response, cancellation).await?;
            if let Some(reason) = http_error::terminal_reason(&bytes) {
                return Err(terminal_quota_failure(reason, http_error::diagnostic("Responses", code, &bytes, &[&bearer])));
            }
            let kind = http_error::compatible_status_kind(
                code,
                retry_after,
                is_context_overflow(&String::from_utf8_lossy(&bytes)),
                &bytes,
            );
            return Err(http_error::mark_bare_bad_request(attach_retry_after(
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
            ), code, &bytes));
        }
        let mut reader = SseReader::new(response);
        let mut batch = TextBatch::new(on_text.clone());
        let mut text = String::new();
        let mut thinking = String::new();
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
                    "response.reasoning_summary_part.added" if !thinking.is_empty() => {
                        thinking.push_str(stream::THINKING_BREAK);
                        batch.push_thinking(stream::THINKING_BREAK);
                    }
                    "response.reasoning_summary_text.delta" => {
                        let delta = event["delta"].as_str().unwrap_or("");
                        thinking.push_str(delta);
                        batch.push_thinking(delta);
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
        let usage = reply_facts::responses_usage(&response);
        let malformed_call = |message: &str| {
            let error = anyhow!(message.to_owned());
            let error = match usage.as_ref() {
                Some(usage) => metered_model_error(error, Some(usage.clone())),
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
        for item in provider_state.as_mut().unwrap()["items"]
            .as_array_mut()
            .unwrap()
        {
            match item["type"].as_str().unwrap_or("") {
                "function_call" => {
                    let id = item["call_id"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| malformed_call("function call missing call_id"))?
                        .to_owned();
                    let name = item["name"]
                        .as_str()
                        .filter(|name| !name.is_empty())
                        .ok_or_else(|| malformed_call("function call missing name"))?
                        .to_owned();
                    if !seen.insert(id.clone()) {
                        return Err(malformed_call("duplicate provider call_id"));
                    }
                    // Enforce tools-off rounds before passing unknown names to core.
                    if !tool_wire::accepts_call(&name, body["tool_choice"] != "none") {
                        return Err(malformed_call("provider returned a tool with tools disabled"));
                    }
                    // The wire envelope requires a string field. Core handles
                    // errors in the JSON text that field carries.
                    let raw = item["arguments"]
                        .as_str()
                        .ok_or_else(|| malformed_call("invalid function call arguments"))?;
                    let arguments = match serde_json::from_str::<Value>(raw) {
                        Ok(arguments) => arguments,
                        Err(_) if cut => continue,
                        Err(_) => {
                            let arguments = tool_wire::raw_arguments(raw);
                            // Keep the replay item aligned with the bounded canonical argument.
                            // Its wire field contains the JSON encoding of that value.
                            item["arguments"] = Value::String(arguments.to_string());
                            arguments
                        }
                    };
                    calls.push(ToolCall {
                        id,
                        name,
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
        if thinking.is_empty() {
            // No summary deltas streamed: read the summaries of the finished reasoning items.
            let summaries = provider_state.as_ref().unwrap()["items"].as_array().into_iter().flatten()
                .filter(|item| item["type"] == "reasoning")
                .flat_map(|item| item["summary"].as_array().into_iter().flatten())
                .filter_map(|part| part["text"].as_str().filter(|text| !text.is_empty()));
            thinking = summaries.collect::<Vec<_>>().join(stream::THINKING_BREAK);
            if !thinking.is_empty() {
                on_text(StreamEvent::ThinkingDelta(&thinking));
            }
        }
        if !published && !final_text.is_empty() {
            on_text(StreamEvent::TextDelta(&final_text));
        }
        if published && final_text != text {
            return Err(classify(
                ProviderErrorKind::Fatal,
                anyhow!("provider final text conflicts with streamed text"),
            ));
        }
        let facts = reply_facts::responses(&response, !calls.is_empty());
        Ok((
            ModelResponse {
                text: final_text,
                thinking,
                tool_calls: calls,
                last_request_usage: Some(crate::LastRequestUsage {
                    input_tokens: response["usage"]["input_tokens"].as_u64(),
                    output_tokens: response["usage"]["output_tokens"].as_u64(),
                }),
                request_usages: usage.iter().cloned().collect(),
                usage,
                provider_state,
                quota: quota.clone(),
                ..facts
            },
            cut,
        ))
        }.await;
        let result = result.map_err(|error| with_error_quota(error, quota));
        let usage = match &result {
            Ok((response, _)) => response.usage.as_ref(),
            Err(error) => ProviderError::of(error).and_then(|error| error.usage.as_ref()),
        };
        if let Ok((response, ..)) = &result {
            meter.finish_response(response);
        } else {
            meter.finish(usage, true);
        }
        result
    }
}
