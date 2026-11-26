//! Stateless Anthropic Messages adapter. One HTTP request per harness step.
use super::stream::{self, SseReader, THINKING_BREAK, TextBatch};
use super::{
    cancelled, classify, continuation, metered_model_error, validate_url, wire_images,
    with_error_quota,
};
use crate::{
    CacheMode, CancellationToken, Capabilities, ModelRequest, ModelResponse, Provider,
    ProviderError, ProviderErrorKind, Role, StreamCallback, StreamEvent, ToolCall, ToolSurface,
    Usage,
};
use anyhow::{Result, anyhow, ensure};
use async_trait::async_trait;
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
#[path = "messages_fork.rs"]
mod fork;
#[path = "messages_rounds.rs"]
mod rounds;
use fork::unfinished_tool_loop;
use rounds::JoinedRounds;

pub struct MessagesProvider {
    pub(in crate::adapter) client: stream::Client,
    key: String,
    base: String,
    opencode_route: bool,
}
impl MessagesProvider {
    pub fn api_key(key: impl Into<String>, base: impl Into<String>) -> Result<Self> {
        let key = key.into();
        ensure!(!key.is_empty(), "API key is empty");
        let base = base.into();
        validate_url(&base)?;
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
        Self::build(request).map(|(body, _)| body)
    }

    /// The body plus the request message index of every wire block.
    fn build(request: &ModelRequest) -> Result<(Value, Vec<Vec<usize>>)> {
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
                    ("user", vec![json!({"type":"text","text":message.text()})])
                }
                Role::Tool => {
                    let id = message
                        .tool_call_id
                        .as_deref()
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| anyhow!("tool message needs call_id"))?;
                    let text = message.text();
                    let images = wire_images(message);
                    let content = if images.is_empty() {
                        json!(text.as_ref())
                    } else {
                        let mut blocks = vec![json!({"type":"text","text":text.as_ref()})];
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
                        crate::provider_state_replay_compatible(
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
                        message.text().is_empty() && message.provider_state.is_some();
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
                    let mut stored = if let Some(state) = &message.provider_state
                        && compatible
                    {
                        super::replay_state::hydrate(state, &message.text(), &message.tool_calls)?
                    } else {
                        let mut blocks = Vec::new();
                        if !message.text().is_empty() {
                            blocks.push(json!({"type":"text","text":message.text()}));
                        }
                        for call in &message.tool_calls {
                            blocks.push(json!({"type":"tool_use","id":call.id,"name":call.name,"input":call.arguments}));
                        }
                        blocks
                    };
                    super::tool_wire::project_messages_inputs(&mut stored);
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
        Ok((body, sources))
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
        on_text: StreamCallback,
        cancellation: &CancellationToken,
    ) -> Result<Value> {
        let mut reader = SseReader::new(response);
        let mut batch = TextBatch::new(on_text);
        let mut blocks = std::collections::BTreeMap::<u64, Value>::new();
        let mut partial_input = std::collections::BTreeMap::<u64, String>::new();
        let mut usage = serde_json::Map::new();
        let mut stop_reason = Value::Null;
        let mut model = Value::Null;
        let mut published = false;
        let mut thinking_block = None::<u64>;
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
                    "message_start" => {
                        model = event["message"]["model"].clone();
                        merge_usage(&mut usage, &event["message"]["usage"]);
                    }
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
                                let fragment = delta["thinking"].as_str().unwrap_or("");
                                append_field(block, "thinking", fragment);
                                // Blocks of one reply read as paragraphs of the same thinking.
                                if thinking_block
                                    .replace(index)
                                    .is_some_and(|last| last != index)
                                {
                                    batch.push_thinking(THINKING_BREAK);
                                }
                                batch.push_thinking(fragment);
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
                                // Preserve the start block when there are no argument deltas.
                                // Parsing handles invalid text after the stop reason is known.
                                let raw = partial_input.remove(&index).unwrap_or_default();
                                let input = if raw.is_empty() {
                                    Some(block.get("input").cloned().unwrap_or_else(|| json!({})))
                                } else {
                                    serde_json::from_str(&raw).ok()
                                };
                                mark_input(block, input, &raw);
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
        for (index, raw) in partial_input {
            if let Some(block) = blocks.get_mut(&index) {
                mark_input(block, None, &raw);
                block["unfinished_input"] = json!(true);
            }
        }
        Ok(json!({
            "model": model,
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
            let mut thinking = Vec::new();
            let mut calls = Vec::new();
            let mut ids = HashSet::new();
            let mut kept = Vec::new();
            for block in blocks {
                if block["type"] == "tool_use" {
                    ensure!(
                        cut || block["unfinished_input"] != true,
                        "Messages tool input was not closed"
                    );
                    ensure!(
                        block.get("input").is_some(),
                        "Messages tool_use lacks input"
                    );
                    if cut && (block["invalid_input"] == true || !block["input"].is_object()) {
                        continue;
                    }
                }
                // Keep assembly markers off the replay wire.
                let mut stored = block.clone();
                if let Some(object) = stored.as_object_mut() {
                    object.remove("invalid_input");
                    object.remove("unfinished_input");
                }
                // Unsigned reasoning at the output limit stays in storage only.
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
                            super::tool_wire::accepts_call(name, !request.tools.is_empty()),
                            "Messages model returned a tool with tools disabled"
                        );
                        calls.push(ToolCall {
                            id: id.into(),
                            name: name.into(),
                            arguments: block["input"].clone(),
                        });
                    }
                    Some("thinking") => {
                        thinking.extend(block["thinking"].as_str().filter(|text| !text.is_empty()))
                    }
                    Some("redacted_thinking") => {}
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
                thinking: thinking.join(THINKING_BREAK),
                tool_calls: calls,
                last_request_usage: Some(crate::LastRequestUsage {
                    input_tokens: raw_usage["input_tokens"]
                        .as_u64()
                        .map(|_| usage.input_tokens),
                    output_tokens: raw_usage["output_tokens"].as_u64(),
                }),
                request_usages: vec![usage.clone()],
                usage: Some(usage.clone()),
                provider_state: Some(
                    json!({"adapter":"anthropic-messages","content":kept,"stop_reason":payload["stop_reason"],"cache_creation_input_tokens":cache_creation}),
                ),
                ..super::reply_facts::messages(&payload)
            })
        })();
        parsed.map_err(|error| metered_model_error(error, raw_usage.is_object().then_some(usage)))
    }
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

/// Stores a tool block's parsed input, or marks the block when the input is not JSON.
fn mark_input(block: &mut Value, input: Option<Value>, raw: &str) {
    block["invalid_input"] = json!(input.is_none());
    block["input"] = input.unwrap_or_else(|| super::tool_wire::raw_arguments(raw));
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
    fn supports_summary_fork(&self, _model: &str) -> bool {
        true
    }
    fn summary_fork_mapping(&self, request: &ModelRequest) -> Result<Option<Value>> {
        Self::fork_mapping(request).map(Some)
    }
    async fn generate_summary_fork(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        super::tool_wire::generate_summary_fork(self, request, cancellation).await
    }
    fn estimate_input_tokens(
        &self,
        request: &ModelRequest,
        count: &dyn Fn(&Value) -> usize,
    ) -> Result<Option<usize>> {
        Self::token_input(request).map(|input| Some(count(&input)))
    }
    fn wire_shape(&self, request: &ModelRequest) -> Option<crate::WireShape> {
        // `body` is pure and is exactly what `generate` posts for this request.
        let body = Self::body(request).ok()?;
        let mut shape =
            crate::WireShape::from_body(&body, "messages", crate::DEFAULT_CACHE_BLOCK_TOKENS);
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
        on_text: StreamCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let (on_text, published) = super::track_text_publication(on_text);
        let (mut known_usage, mut known_quota) = (None::<Usage>, Vec::new());
        let result = async {
            let route_key = self
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
                    let reply = self
                        .post(&body, route_key.as_deref(), &cancellation)
                        .await?;
                    if !self.opencode_route {
                        quota = crate::adapter::quota_headers::parse_anthropic_headers(
                            "anthropic-messages",
                            &crate::current_account()
                                .unwrap_or_else(|| "anthropic-messages".to_owned()),
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
                let result = result.map_err(|error| with_error_quota(error, quota));
                meter.finish_borrowed_result(result.as_ref().map(|(response, ..)| response));
                let (response, cut, streamed) = result?;
                if let Some(usage) = &response.usage {
                    known_usage.get_or_insert_with(Usage::default).add(usage);
                }
                known_quota.extend(response.quota.iter().cloned());
                if !streamed && !response.thinking.is_empty() {
                    on_text(StreamEvent::ThinkingDelta(&response.thinking));
                }
                if !streamed && !response.text.is_empty() {
                    on_text(StreamEvent::TextDelta(&response.text));
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
                    response.provider_state = Some(crate::with_provider_state_origin(
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
#[path = "messages_tests.rs"]
mod tests;
