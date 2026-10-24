//! Stateless Anthropic Messages adapter. One HTTP request per harness step.
use super::stream::{self, SseReader, TextBatch};
use super::{cancelled, classify, continuation, decisions};
use anyhow::{Result, anyhow, ensure};
use async_trait::async_trait;
use bubblepaw_core::{
    CacheMode, CancellationToken, Capabilities, ModelRequest, ModelResponse, Provider,
    ProviderError, ProviderErrorKind, Role, TextCallback, ToolCall, ToolSurface, Usage,
};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::atomic::Ordering;
#[path = "messages_http.rs"]
mod http;

/// Marks a stored thinking block cut by `max_tokens` before its signature. It is
/// kept with the reply for storage and is never sent back to the provider.
const UNSIGNED_CUT: &str = "unsigned_cut";
#[path = "messages_cache.rs"]
mod cache;

pub struct MessagesProvider {
    client: reqwest::Client,
    key: String,
    base: String,
    opencode_route: bool,
}
impl MessagesProvider {
    pub fn api_key(key: impl Into<String>, base: impl Into<String>) -> Result<Self> {
        let key = key.into();
        ensure!(!key.is_empty(), "API key is empty");
        let base = base.into();
        decisions::validate_url(&base)?;
        Ok(Self {
            client: stream::client()?,
            key,
            base: base.trim_end_matches('/').into(),
            opencode_route: false,
        })
    }

    /// Marks this provider as an OpenCode Go route, which requires the
    /// `x-opencode-session` header. Set only from OpenCode provider aliases.
    pub fn with_opencode_route(mut self) -> Self {
        self.opencode_route = true;
        self
    }

    fn body(request: &ModelRequest) -> Result<Value> {
        ensure!(!request.model.is_empty(), "model must be selected");
        ensure!(
            request.max_output_tokens != Some(0),
            "max_tokens must be positive"
        );
        // Only the frozen system prompt goes in `system`, so a view or cold
        // note change leaves it intact. Other system messages are user text.
        let mut system = Vec::new();
        if !request.system_prompt.is_empty() {
            system.push(json!({"type":"text","text":request.system_prompt}));
        }
        let mut messages: Vec<Value> = Vec::new();
        let mut sources: Vec<Vec<usize>> = Vec::new();
        let loop_start = unfinished_tool_loop(&request.messages);
        for (index, message) in request.messages.iter().enumerate() {
            let (role, content) = match message.role {
                Role::System | Role::User => {
                    ("user", vec![json!({"type":"text","text":message.content})])
                }
                Role::Tool => {
                    let id = message
                        .tool_call_id
                        .as_deref()
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| anyhow!("tool message needs call_id"))?;
                    let (text, images) = crate::images::split_wire(&message.content);
                    let content = if images.is_empty() {
                        json!(text)
                    } else {
                        let mut blocks = vec![json!({"type":"text","text":text})];
                        blocks.extend(images.into_iter().map(|image| json!({"type":"image","source":{"type":"base64","media_type":image["mime_type"],"data":image["data"]}})));
                        json!(blocks)
                    };
                    (
                        "user",
                        vec![
                            json!({"type":"tool_result","tool_use_id":id,"content":content,"is_error":message.is_error}),
                        ],
                    )
                }
                Role::Assistant => {
                    let compatible = message.provider_state.as_ref().is_some_and(|state| {
                        bubblepaw_core::provider_state_replay_compatible(
                            state,
                            "anthropic-messages",
                            &request.model,
                        )
                    });
                    // A current tool turn must replay its original native
                    // content, even when adaptive thinking produced none.
                    // Canonical text/calls cannot tell us whether thinking
                    // was lost. Older completed loops may still use fallback.
                    let current_loop = loop_start.is_some_and(|start| index >= start);
                    let reasoning_only =
                        message.content.is_empty() && message.provider_state.is_some();
                    let cause = if message.provider_state.is_none() {
                        "missing provider_state; the caller must carry the original assistant replay state"
                    } else {
                        "incompatible provider_state adapter/model; current-loop state cannot be replayed to this origin"
                    };
                    ensure!(
                        !current_loop
                            || (message.tool_calls.is_empty() && !reasoning_only)
                            || compatible,
                        "Messages current thinking tool loop needs compatible replay state for assistant message {}: {cause}",
                        message.id,
                    );
                    let stored = if let Some(state) = &message.provider_state
                        && compatible
                    {
                        super::replay_state::hydrate(state, &message.content, &message.tool_calls)?
                    } else {
                        let mut blocks = Vec::new();
                        if !message.content.is_empty() {
                            blocks.push(json!({"type":"text","text":message.content}));
                        }
                        for call in &message.tool_calls {
                            blocks.push(json!({"type":"tool_use","id":call.id,"name":call.name,"input":call.arguments}));
                        }
                        blocks
                    };
                    // Withheld blocks stay in storage only; they are never sent.
                    let stored_count = stored.len();
                    let content: Vec<Value> = stored
                        .into_iter()
                        .filter(|block| block[UNSIGNED_CUT].as_bool() != Some(true))
                        .collect();
                    if content.is_empty()
                        && message.provider_state.is_some()
                        && (!compatible || stored_count > 0)
                    {
                        // Foreign reasoning-only records remain canonical but
                        // contribute no invalid empty assistant block on wire.
                        // A record whose only blocks are withheld is skipped too.
                        continue;
                    }
                    ensure!(
                        !content.is_empty(),
                        "assistant message has no content blocks"
                    );
                    ("assistant", content)
                }
            };
            // Anthropic groups parallel results in a single user turn. Merge
            // adjacent same-role messages without modifying any saved blocks.
            if messages.last().is_none_or(|last| last["role"] != role) {
                messages.push(json!({"role":role,"content":[]}));
                sources.push(Vec::new());
            }
            let turn = messages.last_mut().unwrap()["content"].as_array_mut();
            cache::merge(turn.unwrap(), sources.last_mut().unwrap(), content, index);
        }
        ensure!(!messages.is_empty(), "Messages request needs user history");
        ensure!(
            messages.last().unwrap()["role"] == "user",
            "Messages provider does not support assistant prefill"
        );
        let mut body = json!({
            "model":request.model,"system":system,"messages":messages,
            "max_tokens":request.max_output_tokens.unwrap_or_else(|| messages_output_ceiling(&request.model)),"stream":true,
            "thinking":{"type":"adaptive"},"output_config":{"effort":request.reasoning_effort.unwrap_or_default().as_str()}
        });
        // Signed thinking also binds the declaration prefix. Disabling calls
        // keeps those declarations and changes only tool_choice, which is not
        // part of that prefix. Only runtime submission bypasses inactive declarations.
        let tools = if request.tools.is_empty() {
            &request.disabled_tools
        } else {
            &request.tools
        };
        if !tools.is_empty() {
            body["tools"] = json!(tools.iter().map(|tool|json!({"name":tool.name,"description":tool.description,"input_schema":tool.input_schema})).collect::<Vec<_>>());
            body["tool_choice"] =
                json!({"type":if request.tools.is_empty() {"none"} else {"auto"}});
        }
        cache::place(&mut body, &sources, request.cache_tiers.as_ref());
        Ok(body)
    }

    /// Use the actual renderer and its native replay guards, then omit transport
    /// controls and opaque artifacts from the host's local semantic estimate.
    fn token_input(request: &ModelRequest) -> Result<Value> {
        let mut input = Self::body(request)?;
        input
            .as_object_mut()
            .unwrap()
            .retain(|key, _| matches!(key.as_str(), "system" | "messages" | "tools"));
        for message in input["messages"].as_array_mut().unwrap() {
            if let Some(blocks) = message["content"].as_array_mut() {
                for block in blocks {
                    let opaque = match block["type"].as_str() {
                        Some("thinking") => Some("signature"),
                        Some("redacted_thinking") => Some("data"),
                        _ => None,
                    };
                    if let Some(opaque) = opaque
                        && let Some(fields) = block.as_object_mut()
                    {
                        fields.remove(opaque);
                    }
                }
            }
        }
        Ok(input)
    }

    /// Reads a Messages event stream and rebuilds the payload that a JSON reply
    /// would carry: stop reason, usage and content blocks. Text reaches the
    /// listener in batches; thinking blocks and signatures are kept byte-identical.
    async fn read_stream(
        response: reqwest::Response,
        on_text: TextCallback,
        cancellation: &CancellationToken,
    ) -> Result<Value> {
        let mut reader = SseReader::new(response);
        let mut batch = TextBatch::new(on_text);
        let mut blocks = std::collections::BTreeMap::<u64, Value>::new();
        let mut partial_input = std::collections::BTreeMap::<u64, String>::new();
        let mut usage = serde_json::Map::new();
        let mut stop_reason = Value::Null;
        let mut published = false;
        let outcome = stream::pump(
            &mut reader,
            &mut batch,
            cancellation,
            "Messages stream ended before message_stop",
            |data, batch| {
                let event: Value = serde_json::from_str(data).map_err(|_| {
                    stream::failed(
                        ProviderErrorKind::Fatal,
                        "Messages stream event is not JSON",
                    )
                })?;
                match event["type"].as_str().unwrap_or("") {
                    "message_start" => merge_usage(&mut usage, &event["message"]["usage"]),
                    "content_block_start" => {
                        let index = event["index"].as_u64().unwrap_or(0);
                        blocks.insert(index, event["content_block"].clone());
                    }
                    "content_block_delta" => {
                        let index = event["index"].as_u64().unwrap_or(0);
                        let delta = &event["delta"];
                        let block = blocks.entry(index).or_insert_with(|| json!({}));
                        match delta["type"].as_str().unwrap_or("") {
                            "text_delta" => {
                                let fragment = delta["text"].as_str().unwrap_or("");
                                append_field(block, "text", fragment);
                                batch.push(fragment);
                                published |= !fragment.is_empty();
                            }
                            "thinking_delta" => {
                                append_field(
                                    block,
                                    "thinking",
                                    delta["thinking"].as_str().unwrap_or(""),
                                );
                            }
                            "signature_delta" => {
                                append_field(
                                    block,
                                    "signature",
                                    delta["signature"].as_str().unwrap_or(""),
                                );
                            }
                            "input_json_delta" => {
                                partial_input
                                    .entry(index)
                                    .or_default()
                                    .push_str(delta["partial_json"].as_str().unwrap_or(""));
                            }
                            _ => {}
                        }
                    }
                    "content_block_stop" => {
                        let index = event["index"].as_u64().unwrap_or(0);
                        if let Some(block) = blocks.get_mut(&index) {
                            if block["type"] == "tool_use" {
                                // Invalid input is marked, not failed here: a length stop
                                // cuts tool input, and parse decides once the stop is known.
                                let raw = partial_input.remove(&index).unwrap_or_default();
                                let input = if raw.is_empty() {
                                    Some(json!({}))
                                } else {
                                    serde_json::from_str(&raw).ok()
                                };
                                mark_input(block, input);
                            }
                            if block["type"] == "text" {
                                batch.flush();
                            }
                        }
                    }
                    "message_delta" => {
                        if !event["delta"]["stop_reason"].is_null() {
                            stop_reason = event["delta"]["stop_reason"].clone();
                        }
                        merge_usage(&mut usage, &event["usage"]);
                    }
                    "message_stop" => return Ok(true),
                    "error" => {
                        if let Some(reason) = super::http_error::terminal_reason(data.as_bytes()) {
                            return Err(stream::failed(
                                ProviderErrorKind::Fatal,
                                &format!("{reason}: Messages stream rejected"),
                            ));
                        }
                        let kind = match event["error"]["type"].as_str() {
                            Some("authentication_error" | "permission_error") => {
                                ProviderErrorKind::Auth
                            }
                            Some("rate_limit_error") => {
                                ProviderErrorKind::RateLimit { resets_at: None }
                            }
                            Some(
                                "api_error" | "timeout_error" | "overloaded_error"
                                | "conflict_error",
                            ) => ProviderErrorKind::Transient,
                            Some("invalid_request_error")
                                if messages_context_overflow(data.as_bytes()) =>
                            {
                                ProviderErrorKind::Context
                            }
                            _ => ProviderErrorKind::Fatal,
                        };
                        return Err(stream::failed(kind, "Messages stream reported an error"));
                    }
                    _ => {}
                }
                Ok(false)
            },
        )
        .await;
        if let Err(error) = outcome {
            // Usage reported before the failure is billable, so the error keeps it.
            let reported = (!usage.is_empty()).then(|| usage_from(&Value::Object(usage)));
            return Err(with_usage(stream::stopped(error, published), reported));
        }
        // A tool input still open at message_stop was cut mid-stream.
        for (index, _) in partial_input {
            if let Some(block) = blocks.get_mut(&index) {
                mark_input(block, None);
            }
        }
        Ok(json!({
            "stop_reason": stop_reason,
            "usage": Value::Object(usage),
            "content": blocks.into_values().collect::<Vec<_>>(),
        }))
    }

    fn parse(payload: Value, request: &ModelRequest) -> Result<ModelResponse> {
        let raw_usage = &payload["usage"];
        let usage = usage_from(raw_usage);
        let cache_creation = usage.cache_creation_tokens;
        let parsed = (|| {
            // A length stop is a partial reply, which generate continues or returns.
            let cut = match payload["stop_reason"].as_str() {
                Some("refusal") => {
                    return Err(anyhow!(
                        "Messages model refused the request (stop_reason=refusal)"
                    ));
                }
                Some("max_tokens") => true,
                Some("end_turn" | "tool_use" | "stop_sequence") => false,
                _ => {
                    return Err(anyhow!(
                        "Messages model returned an unsupported or missing stop_reason"
                    ));
                }
            };
            let blocks = payload["content"]
                .as_array()
                .ok_or_else(|| anyhow!("Messages response lacks content blocks"))?;
            let mut text = String::new();
            let mut calls = Vec::new();
            let mut ids = HashSet::new();
            let mut kept = Vec::new();
            for block in blocks {
                if block["type"] == "tool_use"
                    && (block["invalid_input"] == true || !block["input"].is_object())
                {
                    // A cut tool call is dropped; a complete one with bad input is an error.
                    if !cut {
                        return Err(classify(
                            ProviderErrorKind::Fatal,
                            anyhow!("Messages tool input is not JSON"),
                        ));
                    }
                    continue;
                }
                // A thinking block cut before its signature is incomplete reasoning.
                // It is stored with the reply, marked so no request sends it back.
                let mut stored = block.clone();
                if cut
                    && block["type"] == "thinking"
                    && block["signature"].as_str().is_none_or(str::is_empty)
                {
                    stored[UNSIGNED_CUT] = json!(true);
                }
                kept.push(stored);
                match block["type"].as_str() {
                    Some("text") => text.push_str(
                        block["text"]
                            .as_str()
                            .ok_or_else(|| anyhow!("Messages text block lacks text"))?,
                    ),
                    Some("tool_use") => {
                        let id = block["id"]
                            .as_str()
                            .filter(|v| !v.is_empty())
                            .ok_or_else(|| anyhow!("Messages tool_use lacks id"))?;
                        let name = block["name"]
                            .as_str()
                            .filter(|v| !v.is_empty())
                            .ok_or_else(|| anyhow!("Messages tool_use lacks name"))?;
                        ensure!(ids.insert(id.to_owned()), "Messages duplicate tool_use id");
                        ensure!(
                            super::tool_wire::accepts_request_call(request, name),
                            "Messages model returned an unavailable tool"
                        );
                        ensure!(
                            block["input"].is_object(),
                            "Messages tool_use input must be an object"
                        );
                        calls.push(ToolCall {
                            id: id.into(),
                            name: name.into(),
                            arguments: block["input"].clone(),
                        });
                    }
                    Some("thinking" | "redacted_thinking") => {}
                    _ => {
                        return Err(anyhow!(
                            "Messages response has an unsupported content block"
                        ));
                    }
                }
            }
            ensure!(
                cut || payload["stop_reason"] != "tool_use" || !calls.is_empty(),
                "Messages tool_use stop has no tool calls"
            );
            ensure!(
                cut || calls.is_empty() || payload["stop_reason"] == "tool_use",
                "Messages tool calls have an inconsistent stop_reason"
            );
            ensure!(
                cut || !text.is_empty() || !calls.is_empty(),
                "Messages model returned no text or tool calls"
            );
            Ok(ModelResponse {
                text,
                tool_calls: calls,
                last_request_usage: Some(bubblepaw_core::LastRequestUsage {
                    input_tokens: raw_usage["input_tokens"]
                        .as_u64()
                        .map(|_| usage.input_tokens),
                    output_tokens: raw_usage["output_tokens"].as_u64(),
                }),
                usage: Some(usage.clone()),
                quota: Vec::new(),
                provider_state: Some(
                    json!({"adapter":"anthropic-messages","content":kept,"stop_reason":payload["stop_reason"],"cache_creation_input_tokens":cache_creation}),
                ),
            })
        })();
        parsed.map_err(|error| {
            decisions::metered_model_error(error, raw_usage.is_object().then_some(usage))
        })
    }
}

/// New user tasks and completed assistant answers close earlier tool loops.
/// Generated host notices are observations within the current loop.
/// Separate reasoning-only records can precede the first call of an open loop.
fn unfinished_tool_loop(messages: &[bubblepaw_core::Message]) -> Option<usize> {
    let mut start = None;
    let mut has_calls = false;
    for (index, message) in messages.iter().enumerate() {
        match message.role {
            Role::User if message.origin != Some(bubblepaw_core::MessageOrigin::Generated) => {
                start = None;
                has_calls = false;
            }
            Role::Assistant if !message.tool_calls.is_empty() => {
                start.get_or_insert(index);
                has_calls = true;
            }
            Role::Assistant if !message.content.is_empty() => {
                start = None;
                has_calls = false;
            }
            Role::Assistant => {
                start.get_or_insert(index);
            }
            _ => {}
        }
    }
    has_calls.then_some(start).flatten()
}

/// Largest synchronous output a model accepts, used when a role sets no
/// `max_output_tokens`. claude-haiku-5-5 is 128K in the Anthropic models
/// overview; other models fall back to 64000, which every listed model accepts.
/// Unknown models are not gated here; the provider decides whether to accept them.
fn messages_output_ceiling(model: &str) -> u32 {
    match model.rsplit('/').next().unwrap_or(model) {
        "claude-haiku-5-5" => 128_000,
        _ => 64_000,
    }
}

/// Copies every reported usage field. Stream usage is cumulative, so later values win.
fn merge_usage(usage: &mut serde_json::Map<String, Value>, source: &Value) {
    if let Some(fields) = source.as_object() {
        for (name, value) in fields {
            usage.insert(name.clone(), value.clone());
        }
    }
}

/// Usage from one Messages usage object. Cache reads and writes count as input.
fn usage_from(raw: &Value) -> Usage {
    let cache_read = raw["cache_read_input_tokens"].as_u64().unwrap_or(0);
    let cache_creation = raw["cache_creation_input_tokens"].as_u64().unwrap_or(0);
    Usage {
        input_tokens: raw["input_tokens"]
            .as_u64()
            .unwrap_or(0)
            .saturating_add(cache_read)
            .saturating_add(cache_creation),
        output_tokens: raw["output_tokens"].as_u64().unwrap_or(0),
        cache_read_tokens: cache_read,
        cache_creation_tokens: cache_creation,
        reasoning_tokens: raw["reasoning_tokens"].as_u64(),
    }
}

/// Attaches usage metered before a stream failed, keeping the classified error.
fn with_usage(error: anyhow::Error, usage: Option<Usage>) -> anyhow::Error {
    let Some(usage) = usage else {
        return error;
    };
    let typed = ProviderError::of(&error)
        .cloned()
        .unwrap_or_else(|| ProviderError::new(ProviderErrorKind::Fatal, error.to_string()));
    error.context(typed.with_usage(usage))
}

/// The rounds of one reply that a length stop split. A single round is
/// returned unchanged; several rounds join their text, usage and content.
#[derive(Default)]
struct JoinedRounds {
    text: String,
    usage: Usage,
    content: Vec<Value>,
    rounds: usize,
    quota: Vec<bubblepaw_core::QuotaReading>,
}

impl JoinedRounds {
    fn push(&mut self, response: &ModelResponse) {
        self.quota.extend(response.quota.iter().cloned());
        self.rounds += 1;
        self.text.push_str(&response.text);
        if let Some(usage) = &response.usage {
            self.usage.add(usage);
        }
        if let Some(blocks) = response
            .provider_state
            .as_ref()
            .and_then(|state| state["content"].as_array())
        {
            self.content.extend(blocks.iter().cloned());
        }
    }

    fn finish(self, last: ModelResponse) -> ModelResponse {
        if self.rounds <= 1 {
            return last;
        }
        let stop_reason = last
            .provider_state
            .as_ref()
            .map(|state| state["stop_reason"].clone())
            .unwrap_or(Value::Null);
        ModelResponse {
            text: self.text,
            tool_calls: last.tool_calls,
            last_request_usage: last.last_request_usage,
            usage: Some(self.usage.clone()),
            quota: self.quota,
            provider_state: Some(json!({
                "adapter": "anthropic-messages",
                "content": self.content,
                "stop_reason": stop_reason,
                "cache_creation_input_tokens": self.usage.cache_creation_tokens,
            })),
        }
    }
}

/// Stores a tool block's parsed input, or marks the block when the input is not JSON.
fn mark_input(block: &mut Value, input: Option<Value>) {
    match input {
        Some(input) => block["input"] = input,
        None => {
            block["input"] = Value::Null;
            block["invalid_input"] = json!(true);
        }
    }
}

/// Appends a streamed fragment to a string field, creating the field when absent.
fn append_field(block: &mut Value, name: &str, fragment: &str) {
    if !block[name].is_string() {
        block[name] = json!("");
    }
    if let Value::String(text) = &mut block[name] {
        text.push_str(fragment);
    }
}

fn messages_context_overflow(bytes: &[u8]) -> bool {
    let body = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    super::is_context_overflow(&body)
        || body.contains("prompt is too long")
        || body.contains("too many input tokens")
}

#[async_trait]
impl Provider for MessagesProvider {
    fn estimate_input_tokens(
        &self,
        request: &ModelRequest,
        count: &dyn Fn(&Value) -> usize,
    ) -> Result<Option<usize>> {
        Self::token_input(request).map(|input| Some(count(&input)))
    }
    fn wire_shape(&self, request: &ModelRequest) -> Option<bubblepaw_core::WireShape> {
        // `body` is pure and is exactly what `generate` posts for this request.
        let body = Self::body(request).ok()?;
        let mut shape = bubblepaw_core::WireShape::from_body(
            &body,
            "messages",
            bubblepaw_core::DEFAULT_CACHE_BLOCK_TOKENS,
        );
        shape.model = request.model.clone();
        Some(shape)
    }
    fn capabilities(&self, _model: &str) -> Capabilities {
        Capabilities {
            context_window: None,
            cache: CacheMode::Prefix,
            tool_surface: ToolSurface::Direct,
            stateful: false,
            image_input: true,
        }
    }
    async fn generate(
        &self,
        request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let (on_text, published) = super::track_text_publication(on_text);
        let (mut known_usage, mut known_quota) = (None::<Usage>, Vec::new());
        let result = async {
            let session = self
                .opencode_route
                .then(|| super::opencode_session(&request));
            let mut request = request;
            let mut joined = JoinedRounds::default();
            let (mut continuations, mut breakpoints) = (0, None);
            loop {
                let body = Self::body(&request)
                    .map_err(|error| classify(ProviderErrorKind::Fatal, error))?;
                breakpoints.get_or_insert_with(|| cache::breakpoints(&body));
                let mut meter = super::PhysicalRequest::new("anthropic-messages", &request.model);
                let mut quota = Vec::new();
                let result: Result<(ModelResponse, bool, bool)> = async {
                    let reply = self.post(&body, session.as_deref(), &cancellation).await?;
                    if !self.opencode_route {
                        let identity =
                            bubblepaw_core::RequestUsage::new("anthropic-messages", &request.model);
                        quota = crate::quota_sources::parse_anthropic_headers(
                            &identity.provider,
                            &identity.account,
                            reply.headers(),
                            chrono::Utc::now(),
                        );
                    }
                    let streamed = stream::is_event_stream(&reply);
                    let payload = if streamed {
                        Self::read_stream(reply, on_text.clone(), &cancellation).await?
                    } else {
                        let bytes = stream::read_body(reply, &cancellation).await?;
                        serde_json::from_slice(&bytes).map_err(|_| {
                            classify(
                                ProviderErrorKind::Fatal,
                                anyhow!("Messages response is not JSON"),
                            )
                        })?
                    };
                    let cut = payload["stop_reason"] == "max_tokens";
                    Self::parse(payload, &request).map(|mut response| {
                        response.quota.extend(quota.clone());
                        (response, cut, streamed)
                    })
                }
                .await;
                let result =
                    result.map_err(|error| crate::quota_sources::with_error_quota(error, quota));
                let usage = match &result {
                    Ok((response, _, _)) => response.usage.as_ref(),
                    Err(error) => ProviderError::of(error).and_then(|error| error.usage.as_ref()),
                };
                meter.finish(usage, result.is_err());
                let (response, cut, streamed) = result?;
                if let Some(usage) = &response.usage {
                    known_usage.get_or_insert_with(Usage::default).add(usage);
                }
                known_quota.extend(response.quota.iter().cloned());
                if !streamed && !response.text.is_empty() {
                    on_text(&response.text);
                }
                joined.push(&response);
                if continuation::should_continue(
                    cut,
                    !response.tool_calls.is_empty(),
                    continuations,
                ) {
                    request = continuation::continue_response(&request, &response);
                    continuations += 1;
                    continue;
                }
                if cut {
                    continuation::warn_incomplete(
                        "anthropic-messages",
                        &request.model,
                        continuations,
                    );
                }
                let mut response = joined.finish(response);
                if let Some(state) = response.provider_state.take() {
                    response.provider_state = Some(bubblepaw_core::with_provider_state_origin(
                        super::replay_state::slim(state, &response.text, &response.tool_calls)?,
                        "anthropic-messages",
                        Some(&request.model),
                        "native",
                    )?);
                }
                return Ok(cache::report(response, breakpoints.take()));
            }
        }
        .await;
        result.map_err(|error| {
            super::mark_published(
                super::retain_prior_quota(
                    super::retain_prior_usage(error, known_usage.as_ref()),
                    known_quota,
                ),
                published.load(Ordering::Acquire),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bubblepaw_core::{Message, ToolDefinition};

    fn message(role: Role, content: &str) -> Message {
        Message {
            id: "fixture-message".into(),
            session_id: "fixture-session".into(),
            role,
            origin: None,
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: String::new(),
        }
    }

    fn tool_turn() -> Message {
        let mut assistant = message(Role::Assistant, "Reading the synthetic path.");
        assistant.tool_calls = vec![ToolCall {
            id: "call-guard".into(),
            name: "read".into(),
            arguments: json!({"path":"synthetic"}),
        }];
        assistant
    }

    fn tool_result() -> Message {
        let mut result = message(Role::Tool, "Synthetic result.");
        result.tool_call_id = Some("call-guard".into());
        result
    }

    fn tool_state(assistant: &Message) -> Value {
        let call = &assistant.tool_calls[0];
        json!({
            "adapter":"anthropic-messages",
            "content":[
                {"type":"text","text":assistant.content},
                {"type":"tool_use","id":call.id,"name":call.name,"input":call.arguments}
            ],
            "stop_reason":"tool_use"
        })
    }

    fn loop_request(assistant: Message) -> ModelRequest {
        ModelRequest {
            model: "claude-haiku-5-5".into(),
            messages: vec![
                message(Role::User, "Read the synthetic path."),
                assistant,
                tool_result(),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn messages_projection_shares_native_guards_and_omits_only_opaque_payloads() -> Result<()> {
        use crate::provider::ProviderRegistry;
        let provider = std::sync::Arc::new(MessagesProvider::api_key(
            "synthetic-key",
            "http://127.0.0.1:1",
        )?);
        let mut assistant = message(Role::Assistant, "done");
        assistant.provider_state = Some(bubblepaw_core::with_provider_state_origin(
            json!({"adapter":"anthropic-messages","content":[
                {"type":"thinking","thinking":"readable thought","signature":"opaque signature".repeat(10_000)},
                {"type":"redacted_thinking","data":"opaque redaction".repeat(10_000)},
                {"type":"thinking","thinking":"withheld thought","signature":"", "unsigned_cut":true},
                {"type":"text","text":"done"}
            ]}),
            "anthropic-messages",
            Some("test"),
            "native",
        )?);
        let mut request = ModelRequest {
            model: "test".into(),
            system_prompt: "stable instructions".into(),
            messages: vec![
                message(Role::User, "task"),
                assistant,
                message(Role::User, "next"),
            ],
            disabled_tools: vec![bubblepaw_core::ToolDefinition {
                name: "read".into(),
                description: "read description".into(),
                input_schema: json!({"type":"object"}),
                read_only: true,
                approval: Default::default(),
            }],
            ..Default::default()
        };
        let original = serde_json::to_value(&request)?;
        let original_body = MessagesProvider::body(&request)?;
        let input = MessagesProvider::token_input(&request)?;
        let rendered = input.to_string();
        assert!(rendered.contains("readable thought"));
        assert!(rendered.contains("read description"));
        assert!(rendered.contains("stable instructions"));
        assert!(!rendered.contains("opaque signature"));
        assert!(!rendered.contains("opaque redaction"));
        assert!(!rendered.contains("withheld thought"));
        assert!(input.get("model").is_none());
        assert!(input.get("max_tokens").is_none());
        assert_eq!(
            provider.estimate_input_tokens(&request, &|value| {
                assert_eq!(value, &input);
                123
            })?,
            Some(123)
        );
        assert_eq!(serde_json::to_value(&request)?, original);
        assert_eq!(MessagesProvider::body(&request)?, original_body);

        request.model = "different-model".into();
        assert!(
            !MessagesProvider::token_input(&request)?
                .to_string()
                .contains("readable thought")
        );
        request.model = "route/test".into();
        request.messages[1].provider_state.as_mut().unwrap()["_bubblepaw_route_origin"] =
            json!({"provider":"route", "endpoint":"http://127.0.0.1:2"});
        let mut registry = ProviderRegistry::new("route", provider.clone());
        registry.configure_route("route", "http://127.0.0.1:1".into(), Default::default());
        assert_eq!(
            registry.estimate_input_tokens(&request, &|value| {
                assert!(!value.to_string().contains("readable thought"));
                456
            })?,
            Some(456)
        );

        let invalid = loop_request(tool_turn());
        let estimate_error = provider
            .estimate_input_tokens(&invalid, &|_| 0)
            .unwrap_err();
        assert_eq!(
            estimate_error.to_string(),
            MessagesProvider::body(&invalid).unwrap_err().to_string()
        );
        Ok(())
    }

    #[test]
    fn thinking_tool_loop_rejects_missing_replay_state() {
        let error = MessagesProvider::body(&loop_request(tool_turn())).unwrap_err();
        assert!(error.to_string().contains("current thinking tool loop"));
        assert!(error.to_string().contains("fixture-message"));
        assert!(error.to_string().contains("missing provider_state"));
    }

    #[test]
    fn thinking_tool_loop_rejects_missing_state_after_generated_host_notice() {
        let mut request = loop_request(tool_turn());
        let mut notice = message(Role::User, "[host notice]\nSynthetic job completed.");
        notice.origin = Some(bubblepaw_core::MessageOrigin::Generated);
        request.messages.push(notice);
        let error = MessagesProvider::body(&request).unwrap_err();
        assert!(error.to_string().contains("current thinking tool loop"));
        assert!(error.to_string().contains("missing provider_state"));
    }

    #[test]
    fn thinking_tool_loop_rejects_incompatible_replay_state() -> Result<()> {
        for (kind, model) in [
            ("responses", "claude-haiku-5-5"),
            ("anthropic-messages", "other-model"),
        ] {
            let mut assistant = tool_turn();
            assistant.provider_state = Some(bubblepaw_core::with_provider_state_origin(
                tool_state(&assistant),
                kind,
                Some(model),
                "synthetic",
            )?);
            let error = MessagesProvider::body(&loop_request(assistant)).unwrap_err();
            assert!(error.to_string().contains("compatible replay state"));
            assert!(
                error
                    .to_string()
                    .contains("incompatible provider_state adapter/model")
            );
        }
        let mut assistant = tool_turn();
        assistant.provider_state = Some(json!({"adapter":"responses","items":[]}));
        assert!(MessagesProvider::body(&loop_request(assistant)).is_err());
        Ok(())
    }

    #[test]
    fn thinking_tool_loop_rejects_foreign_reasoning_only_record() -> Result<()> {
        let mut assistant = tool_turn();
        assistant.provider_state = Some(tool_state(&assistant));
        let mut request = loop_request(assistant);
        let mut reasoning = message(Role::Assistant, "");
        reasoning.id = "reasoning-guard".into();
        reasoning.provider_state = Some(bubblepaw_core::with_provider_state_origin(
            json!({"adapter":"anthropic-messages","content":[
                {"type":"thinking","thinking":"Synthetic plan.","signature":"sig+/==猫"}
            ]}),
            "anthropic-messages",
            Some("other-model"),
            "synthetic",
        )?);
        request.messages.insert(1, reasoning);
        let error = MessagesProvider::body(&request).unwrap_err();
        assert!(error.to_string().contains("reasoning-guard"));
        Ok(())
    }

    #[test]
    fn thinking_tool_loop_checks_every_current_call_turn() {
        let missing = tool_turn();
        let mut valid = tool_turn();
        valid.provider_state = Some(tool_state(&valid));
        let mut request = loop_request(missing);
        request.messages.extend([valid, tool_result()]);
        assert!(MessagesProvider::body(&request).is_err());
    }

    #[test]
    fn thinking_tool_loop_allows_fallback_after_completed_answer_or_new_user() -> Result<()> {
        for state in [
            None,
            Some(json!({"adapter":"responses","items":[
                {"type":"reasoning","encrypted_content":"foreign-opaque"}
            ]})),
        ] {
            for completed_answer in [false, true] {
                let mut assistant = tool_turn();
                assistant.provider_state = state.clone();
                let mut request = loop_request(assistant.clone());
                if completed_answer {
                    request.messages.push(message(Role::Assistant, "Finished."));
                    let mut current = tool_turn();
                    current.provider_state = Some(tool_state(&current));
                    request.messages.extend([current, tool_result()]);
                } else {
                    request.messages.push(message(Role::User, "Next task."));
                }
                let body = MessagesProvider::body(&request)?;
                let expected = tool_state(&assistant)["content"].clone();
                assert_eq!(body["messages"][1]["content"], expected);
                assert!(!body.to_string().contains("foreign-opaque"));
            }
        }
        Ok(())
    }

    #[test]
    fn thinking_tool_loop_accepts_native_reply_without_thinking() -> Result<()> {
        let assistant = tool_turn();
        let mut request = loop_request(assistant.clone());
        request.tools = vec![ToolDefinition {
            name: "read".into(),
            description: "Read synthetic data".into(),
            input_schema: json!({"type":"object"}),
            read_only: true,
            approval: Default::default(),
        }];
        let blocks = tool_state(&assistant)["content"].clone();
        let response = MessagesProvider::parse(
            json!({"stop_reason":"tool_use","content":blocks,"usage":{}}),
            &request,
        )?;
        let state = super::super::replay_state::slim(
            response.provider_state.unwrap(),
            &response.text,
            &response.tool_calls,
        )?;
        request.messages[1].provider_state = Some(bubblepaw_core::with_provider_state_origin(
            state,
            "anthropic-messages",
            Some(&request.model),
            "native",
        )?);
        assert_eq!(
            MessagesProvider::body(&request)?["messages"][1]["content"],
            blocks
        );
        Ok(())
    }

    #[test]
    fn thinking_tool_loop_rejects_malformed_compatible_envelope() {
        let mut assistant = tool_turn();
        assistant.provider_state = Some(json!({"adapter":"anthropic-messages"}));
        let error = MessagesProvider::body(&loop_request(assistant)).unwrap_err();
        assert!(error.to_string().contains("missing replay blocks"));
    }

    #[test]
    fn complete_request_body_is_byte_exact_for_legacy_and_slim_signed_history() -> Result<()> {
        let calls = vec![
            ToolCall {
                id: "call-a".into(),
                name: "read".into(),
                arguments: json!({"path":"ARGUMENT_SENTINEL_A"}),
            },
            ToolCall {
                id: "call-b".into(),
                name: "read".into(),
                arguments: json!({"path":"ARGUMENT_SENTINEL_B"}),
            },
        ];
        let fragments = [
            "FIRST_CANONICAL_TEXT π",
            "SECOND_CANONICAL_TEXT",
            "THIRD_CANONICAL_TEXT",
        ];
        let content = json!([
            {"type":"thinking","thinking":"Synthetic plan.","signature":"sig+/==","opaque":{"keep":true}},
            {"type":"text","text":fragments[0],"extra":"retained"},
            {"type":"tool_use","id":calls[0].id,"name":calls[0].name,"input":calls[0].arguments},
            {"type":"redacted_thinking","data":"synthetic-redacted+/=="},
            {"type":"text","text":fragments[1]},
            {"type":"tool_use","id":calls[1].id,"name":calls[1].name,"input":calls[1].arguments},
            {"type":"text","text":fragments[2]}
        ]);
        let full_state =
            json!({"adapter":"anthropic-messages","content":content,"stop_reason":"tool_use"});
        let mut assistant = message(Role::Assistant, &fragments.concat());
        assistant.tool_calls = calls;
        assistant.provider_state = Some(full_state.clone());
        let mut result_a = message(Role::Tool, "Synthetic first result.");
        result_a.tool_call_id = Some("call-a".into());
        let mut result_b = message(Role::Tool, "Synthetic second error.");
        result_b.tool_call_id = Some("call-b".into());
        result_b.is_error = true;
        let legacy = ModelRequest {
            model: "claude-haiku-5-5".into(),
            system_prompt: "Synthetic instructions.".into(),
            messages: vec![
                message(Role::System, "Additional guidance."),
                message(Role::User, "Read both synthetic paths."),
                assistant,
                result_a,
                result_b,
            ],
            tools: vec![ToolDefinition {
                name: "read".into(),
                description: "Read a path".into(),
                input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
                read_only: true,
                approval: Default::default(),
            }],
            max_output_tokens: Some(128),
            ..Default::default()
        };
        let mut slim = legacy.clone();
        let assistant = &mut slim.messages[2];
        let state = super::super::replay_state::slim(
            full_state,
            &assistant.content,
            &assistant.tool_calls,
        )?;
        let serialized = state.to_string();
        for canonical in fragments.into_iter().chain([
            "ARGUMENT_SENTINEL_A",
            "ARGUMENT_SENTINEL_B",
            "call-a",
            "call-b",
        ]) {
            assert!(!serialized.contains(canonical));
        }
        assistant.provider_state = Some(bubblepaw_core::with_provider_state_origin(
            state,
            "anthropic-messages",
            Some(&slim.model),
            "native",
        )?);
        assert_eq!(
            serde_json::to_vec(&MessagesProvider::body(&legacy)?)?,
            serde_json::to_vec(&MessagesProvider::body(&slim)?)?
        );
        Ok(())
    }
}
