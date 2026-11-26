//! Stateless OpenAI chat-completions provider with native function calls.
use super::stream::{self, SseReader, TextBatch};
use super::{cancelled, classify, continuation, tool_wire};
use anyhow::{Result, anyhow, ensure};
use async_trait::async_trait;
use bubblepaw_core::{
    CacheMode, CancellationToken, Capabilities, ModelRequest, ModelResponse, Provider,
    ProviderError, ProviderErrorKind, ReasoningReplay, Role, StreamCallback, StreamEvent, ToolCall,
    ToolSurface, Usage,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::Ordering;
#[path = "chat_http.rs"]
mod http;
use http::chat_error_kind;

pub(crate) fn is_loopback_url(base: &str) -> bool {
    reqwest::Url::parse(base).ok().is_some_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.host_str().is_some_and(|host| {
                host == "localhost"
                    || host.ends_with(".localhost")
                    || host
                        .trim_matches(['[', ']'])
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            })
    })
}

pub struct ChatCompletionsProvider {
    pub(in crate::provider) client: stream::Client,
    key: String,
    base: String,
    reasoning_fields: Value,
    reasoning_replay: BTreeMap<String, ReasoningReplay>,
    opencode_route: bool,
}
impl ChatCompletionsProvider {
    pub fn api_key(key: impl Into<String>, base: impl Into<String>) -> Result<Self> {
        let key = key.into();
        ensure!(!key.trim().is_empty(), "API key is empty");
        ensure!(
            reqwest::header::HeaderValue::from_str(&format!("Bearer {key}")).is_ok(),
            "API key is not a valid Bearer credential"
        );
        Self::new(key, base.into())
    }
    /// Local OpenAI-compatible servers may deliberately run without authentication.
    pub fn without_key(base: impl Into<String>) -> Result<Self> {
        let base = base.into();
        ensure!(
            is_loopback_url(&base),
            "Keyless chat requires a loopback base URL"
        );
        Self::new(String::new(), base)
    }
    fn new(key: String, base: String) -> Result<Self> {
        super::decisions::validate_url(&base)?;
        Ok(Self {
            client: stream::client()?,
            key,
            base: base.trim_end_matches('/').into(),
            reasoning_fields: json!({"reasoning_effort":"low"}),
            reasoning_replay: BTreeMap::new(),
            opencode_route: false,
        })
    }
    /// Marks this provider as an OpenCode Go route, which requires the
    /// `x-opencode-session` header. Set only from OpenCode provider aliases.
    pub fn with_opencode_route(mut self) -> Self {
        self.opencode_route = true;
        self
    }
    pub fn with_reasoning_fields(mut self, fields: Value) -> Result<Self> {
        Self::validate_reasoning_fields(&fields)?;
        self.reasoning_fields = fields;
        Ok(self)
    }
    /// Exact model IDs select explicit replay; unconfigured models replay the
    /// current tool loop's reasoning.
    pub fn with_reasoning_replay(
        mut self,
        models: BTreeMap<String, ReasoningReplay>,
    ) -> Result<Self> {
        Self::validate_reasoning_replay(&models)?;
        self.reasoning_replay = models;
        Ok(self)
    }
    pub(super) fn validate_reasoning_replay(
        models: &BTreeMap<String, ReasoningReplay>,
    ) -> Result<()> {
        ensure!(
            models.keys().all(|model| !model.is_empty()
                && !model.chars().any(char::is_whitespace)
                && !model.chars().any(char::is_control)),
            "Reasoning replay needs nonempty exact model IDs"
        );
        Ok(())
    }
    fn wire_input(&self, request: &ModelRequest) -> Result<Value> {
        let mut input = json!({"messages": self.wire_messages(request)?});
        tool_wire::insert_tools(&mut input, request, tool_wire::chat_declaration);
        Ok(input)
    }
    fn wire_messages(&self, request: &ModelRequest) -> Result<Vec<Value>> {
        self.wire_messages_mapped(request, false)
            .map(|(messages, _)| messages)
    }
    fn wire_messages_mapped(
        &self,
        request: &ModelRequest,
        map_sources: bool,
    ) -> Result<(Vec<Value>, Vec<Value>)> {
        let policy = self
            .reasoning_replay
            .get(&request.model)
            .copied()
            .unwrap_or_default();
        let loop_start = unfinished_tool_loop(&request.messages);
        let mut messages = vec![json!({"role":"system","content":request.system_prompt})];
        let mut pending_images = Vec::new();
        let mut mapping: Vec<Value> = Vec::new();
        let mut image_sources: Vec<usize> = Vec::new();
        for (index, message) in request.messages.iter().enumerate() {
            if message.role != Role::Tool && !pending_images.is_empty() {
                for source in image_sources.drain(..) {
                    mapping[source]["wire_positions"]
                        .as_array_mut()
                        .unwrap()
                        .push(json!(messages.len()));
                }
                messages.push(chat_image_message(std::mem::take(&mut pending_images)));
            }
            let wire_start = messages.len();
            let role = match message.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            };
            let replay = message.role == Role::Assistant
                && match policy {
                    ReasoningReplay::None => false,
                    ReasoningReplay::All => true,
                    ReasoningReplay::ToolLoop => loop_start.is_some_and(|start| index >= start),
                };
            if replay
                && let Some(state) = message
                    .provider_state
                    .as_ref()
                    .filter(|state| chat_compatible(state, &request.model))
            {
                if let Some(rounds) = state["rounds"]
                    .as_array()
                    .filter(|rounds| !rounds.is_empty())
                {
                    let full = message.text();
                    for round in rounds {
                        let range = round["text_range"].as_array();
                        let text = range
                            .and_then(|range| {
                                Some((
                                    usize::try_from(range.first()?.as_u64()?).ok()?,
                                    usize::try_from(range.get(1)?.as_u64()?).ok()?,
                                ))
                            })
                            .and_then(|(start, end)| full.get(start..end))
                            .unwrap_or(&full);
                        messages.push(chat_wire_message(round, text));
                    }
                } else {
                    messages.push(chat_wire_message(state, &message.text()));
                }
            } else if message.role == Role::Assistant
                && message.text().is_empty()
                && message.tool_calls.is_empty()
                && message.provider_state.as_ref().is_some_and(|state| {
                    state
                        .get(bubblepaw_core::PROVIDER_STATE_ORIGIN_FIELD)
                        .is_some()
                        && !chat_compatible(state, &request.model)
                })
            {
                // Foreign reasoning-only records (imports, other adapters) stay
                // in the store; they add no empty assistant turn on this wire.
                if map_sources {
                    mapping.push(json!({"source_id":message.id,"prepared_position":index,"role":message.role,"origin":message.origin,"is_typed_user":bubblepaw_core::is_typed_user(message),"wire_positions":[],"tool_call_ids":[],"tool_call_id":message.tool_call_id}));
                }
                continue;
            } else {
                messages.push(json!({"role":role,"content":message.text()}));
            }
            // Tool history remains native even when a host stops offering new
            // tools on the final round. Opaque reasoning cannot replace it.
            if message.role == Role::Tool {
                let text = message.text();
                let images = crate::images::wire_images(message);
                if map_sources && !images.is_empty() {
                    image_sources.push(mapping.len());
                }
                pending_images.extend(images);
                messages.last_mut().unwrap()["content"] =
                    json!(tool_wire::error_text(message, &text));
                messages.last_mut().unwrap()["tool_call_id"] = json!(
                    message
                        .tool_call_id
                        .as_deref()
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| anyhow!(
                            "Chat completions tool message needs tool_call_id"
                        ))?
                );
            } else if message.role == Role::Assistant && !message.tool_calls.is_empty() {
                for wire in &mut messages[wire_start..] {
                    wire.as_object_mut().unwrap().remove("tool_calls");
                }
                messages.last_mut().unwrap()["tool_calls"] = Value::Array(message.tool_calls.iter()
                    .map(|call| Ok(json!({"id":call.id,"type":"function","function":{"name":call.name,"arguments":serde_json::to_string(&call.arguments)?}})))
                    .collect::<Result<Vec<_>>>()?);
                if message.text().is_empty() {
                    messages.last_mut().unwrap()["content"] = Value::Null;
                }
            }
            if map_sources {
                mapping.push(json!({
                    "source_id":message.id,"prepared_position":index,"role":message.role,
                    "origin":message.origin,"is_typed_user":bubblepaw_core::is_typed_user(message),
                    "wire_positions":(wire_start..messages.len()).collect::<Vec<_>>(),
                    "tool_call_ids":message.tool_calls.iter().map(|call| &call.id).collect::<Vec<_>>(),
                    "tool_call_id":message.tool_call_id,
                }));
            }
        }
        // Chat tool content is text; a user image turn follows the entire
        // parallel result group, preserving native tool-call adjacency.
        if !pending_images.is_empty() {
            for source in image_sources {
                mapping[source]["wire_positions"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!(messages.len()));
            }
            messages.push(chat_image_message(pending_images));
        }
        Ok((messages, mapping))
    }
    pub(super) fn validate_reasoning_fields(fields: &Value) -> Result<()> {
        ensure!(
            fields.as_object().is_some_and(|object| object
                .keys()
                .all(|key| matches!(key.as_str(), "thinking" | "reasoning_effort"))),
            "Unsupported chat completions reasoning field"
        );
        if let Some(effort) = fields.get("reasoning_effort") {
            super::parse_reasoning_effort(
                effort
                    .as_str()
                    .ok_or_else(|| anyhow!("Reasoning effort must be a string"))?,
            )?;
        }
        if let Some(thinking) = fields.get("thinking") {
            ensure!(
                thinking == "enabled"
                    || thinking.as_object().is_some_and(|_| matches!(
                        thinking["type"].as_str(),
                        Some("enabled" | "adaptive")
                    )),
                "Thinking must be enabled or adaptive"
            );
        }
        Ok(())
    }
}
fn chat_image_message(images: Vec<Value>) -> Value {
    let mut content = vec![json!({"type":"text","text":"Images read by the preceding tools:"})];
    content.extend(images.into_iter().map(|image| json!({"type":"image_url","image_url":{"url":format!("data:{};base64,{}",image["mime_type"].as_str().unwrap_or(""),image["data"].as_str().unwrap_or(""))}})));
    json!({"role":"user","content":content})
}
impl ChatCompletionsProvider {
    /// Reads a server-sent completion and assembles the same payload shape as a
    /// JSON reply. Text reaches the listener in batches as it arrives.
    async fn read_stream(
        response: reqwest::Response,
        on_text: StreamCallback,
        cancellation: &CancellationToken,
    ) -> Result<Value> {
        let mut reader = SseReader::new(response);
        let mut batch = TextBatch::new(on_text);
        let mut text: Option<String> = None;
        let mut reasoning: Option<String> = None;
        let mut openrouter_reasoning: Option<String> = None;
        let mut reasoning_details: Option<Vec<Value>> = None;
        let mut usage = Value::Null;
        let mut finish: Option<String> = None;
        let mut native_finish: Option<String> = None;
        let mut published = false;
        let mut opaque = Vec::new();
        let mut stream_metadata = serde_json::Map::new();
        let mut calls = BTreeMap::<u64, Value>::new();
        let outcome = stream::pump(
            &mut reader,
            &mut batch,
            cancellation,
            "Chat completions stream ended without [DONE]",
            |data, batch| {
                if data == "[DONE]" {
                    return Ok(true);
                }
                let event: Value = serde_json::from_str(data).map_err(|_| {
                    stream::failed(
                        ProviderErrorKind::Fatal,
                        "Chat completions stream JSON is invalid",
                    )
                })?;
                if !event["error"].is_null() {
                    if let Some(reason) = super::http_error::terminal_reason(event.to_string().as_bytes()) {
                        return Err(stream::failed(ProviderErrorKind::Fatal, &format!("{reason}: Chat completions stream rejected")));
                    }
                    return Err(stream::failed(
                        if published { ProviderErrorKind::Fatal } else { chat_error_kind(&event["error"]) },
                        "Chat completions stream reported an error",
                    ));
                }
                if !event["usage"].is_null() {
                    usage = event["usage"].clone();
                }
                if let Some(reason) = event["choices"][0]["finish_reason"].as_str() {
                    finish = Some(reason.to_owned());
                }
                if let Some(reason) = event["choices"][0]["native_finish_reason"].as_str() {
                    native_finish = Some(reason.to_owned());
                }
                let delta = &event["choices"][0]["delta"];
                if let Some(fragments) = delta["tool_calls"].as_array() {
                    for fragment in fragments {
                        let index = fragment["index"].as_u64().ok_or_else(|| stream::failed(
                            ProviderErrorKind::Fatal, "Chat completions tool delta lacks index"))?;
                        let call = calls.entry(index).or_insert_with(|| json!({"id":"","type":"function","function":{"name":"","arguments":""}}));
                        for field in ["id", "type"] {
                            if let Some(value) = fragment.get(field).filter(|value| !value.is_null()) {
                                if field == "id" {
                                    append_call_fragment(&mut call[field], value)?;
                                } else {
                                    call[field] = value.clone();
                                }
                            }
                        }
                        for field in ["name", "arguments"] {
                            if let Some(value) = fragment["function"].get(field).filter(|value| !value.is_null()) {
                                append_call_fragment(&mut call["function"][field], value)?;
                            }
                        }
                    }
                }
                if let Some(fragment) = delta["content"].as_str() {
                    text.get_or_insert_with(String::new).push_str(fragment);
                    batch.push(fragment);
                    published |= !fragment.is_empty();
                }
                if let Some(fragment) = delta["reasoning_content"].as_str() {
                    reasoning.get_or_insert_with(String::new).push_str(fragment);
                    batch.push_thinking(fragment);
                }
                if let Some(fragment) = delta["reasoning"].as_str() {
                    openrouter_reasoning.get_or_insert_with(String::new).push_str(fragment);
                }
                // OpenRouter defines details as an ordered sequence across
                // chunks. Preserve complete objects, including repeated indices
                // and text also present in the independent plain reasoning field.
                if let Some(details) = delta["reasoning_details"].as_array() {
                    reasoning_details.get_or_insert_with(Vec::new).extend(details.iter().cloned());
                }
                // Unknown fragments have no defined assembly rule. Keep each
                // once, in arrival order, without duplicating assembled text
                // or reasoning in the opaque journal.
                let mut unknown = event.clone();
                if let Some(object) = unknown.as_object_mut() {
                    object.remove("usage");
                    object.remove("error");
                    // Completion metadata is repeated on ordinary token frames,
                    // not new opaque model state. Retain its first value once.
                    // If it changes, preserve that frame's differing value in
                    // the ordered journal rather than silently overwriting it.
                    for field in ["id", "object", "created", "model", "system_fingerprint", "service_tier"] {
                        if let Some(value) = object.get(field) {
                            match stream_metadata.get(field) {
                                Some(first) if first == value => { object.remove(field); }
                                None => {
                                    stream_metadata.insert(field.to_owned(), object.remove(field).unwrap());
                                }
                                _ => {}
                            }
                        }
                    }
                    if let Some(choices) = object.get_mut("choices").and_then(Value::as_array_mut) {
                        if let Some(choice) = choices.first_mut().and_then(Value::as_object_mut) {
                            choice.remove("finish_reason");
                            choice.remove("native_finish_reason");
                            choice.remove("index");
                            if let Some(fields) =
                                choice.get_mut("delta").and_then(Value::as_object_mut)
                            {
                                for field in ["content", "reasoning_content", "reasoning"] {
                                    if fields.get(field).is_some_and(Value::is_string) {
                                        fields.remove(field);
                                    }
                                }
                                if fields.get("reasoning_details").is_some_and(Value::is_array) {
                                    fields.remove("reasoning_details");
                                }
                                fields.remove("tool_calls");
                                fields.remove("role");
                                if fields.is_empty() {
                                    choice.remove("delta");
                                }
                            }
                        }
                        if choices
                            .iter()
                            .all(|choice| choice.as_object().is_some_and(serde_json::Map::is_empty))
                        {
                            object.remove("choices");
                        }
                    }
                    if !object.is_empty() {
                        opaque.push(unknown);
                    }
                }
                Ok(false)
            },
        )
        .await;
        if let Err(error) = outcome {
            return Err(stream::stopped(error, published));
        }
        let mut message = json!({"content": text, "reasoning_content": reasoning, "tool_calls":calls.into_values().collect::<Vec<_>>()});
        if let Some(reasoning) = openrouter_reasoning {
            message["reasoning"] = json!(reasoning);
        }
        if let Some(details) = reasoning_details {
            message["reasoning_details"] = json!(details);
        }
        let mut payload = json!({
            "choices": [{"finish_reason": finish, "message": message}],
            "usage": usage,
            "_bubblepaw_stream_opaque": opaque,
        });
        payload.as_object_mut().unwrap().extend(stream_metadata);
        if let Some(reason) = native_finish {
            payload["choices"][0]["native_finish_reason"] = json!(reason);
        }
        Ok(payload)
    }
}

fn chat_state(payload: &Value) -> Value {
    let mut fields = payload["choices"][0]["message"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    fields.remove("content");
    fields.remove("role");
    fields.remove("tool_calls");
    let mut response = payload.as_object().cloned().unwrap_or_default();
    response.remove("choices");
    response.remove("usage");
    let opaque = response.remove("_bubblepaw_stream_opaque");
    let mut choice = payload["choices"][0]
        .as_object()
        .cloned()
        .unwrap_or_default();
    choice.remove("message");
    choice.remove("finish_reason");
    choice.remove("index");
    let mut state = json!({"adapter":"chat-completions","message_fields":fields});
    if !response.is_empty() {
        state["response_fields"] = Value::Object(response);
    }
    if !choice.is_empty() {
        state["choice_fields"] = Value::Object(choice);
    }
    if let Some(opaque) =
        opaque.filter(|value| value.as_array().is_some_and(|values| !values.is_empty()))
    {
        state["opaque_stream_fragments"] = opaque;
    }
    state
}

fn chat_compatible(state: &Value, model: &str) -> bool {
    bubblepaw_core::provider_state_replay_compatible(state, "chat-completions", model)
        // The historical chat envelope predates adapter/origin metadata. It
        // contains only reasoning_content and is usable only by this adapter.
        || (state.get("adapter").is_none() && state.get(bubblepaw_core::PROVIDER_STATE_ORIGIN_FIELD).is_none()
            && state.as_object().is_some_and(|fields| fields.len() == 1 && fields.contains_key("reasoning_content")))
}

/// One assistant wire message from a stored state.
fn chat_wire_message(state: &Value, text: &str) -> Value {
    let mut fields = state["message_fields"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    // A reply without reasoning stores `reasoning_content: null`; send nothing.
    fields.retain(|_, value| !value.is_null());
    if fields.is_empty()
        && let Some(reasoning) = state.get("reasoning_content")
    {
        fields.insert("reasoning_content".into(), reasoning.clone());
    }
    // Canonical role/content always win over opaque extensions. Unknown SSE
    // fragments remain canonical only: inventing an assembly rule would change
    // the provider's artifact, whereas JSON message fields can replay directly.
    fields.insert("role".into(), json!("assistant"));
    fields.insert("content".into(), json!(text));
    Value::Object(fields)
}

/// A completed assistant reply or a new user task closes prior tool loops.
/// Runtime host notices remain part of the current tool loop.
/// Reasoning-only assistant records may precede the call in the same loop.
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
            Role::Assistant if !message.text().is_empty() => {
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

fn append_call_fragment(
    target: &mut Value,
    fragment: &Value,
) -> std::result::Result<(), stream::StreamError> {
    let fragment = fragment.as_str().ok_or_else(|| {
        stream::failed(
            ProviderErrorKind::Fatal,
            "Chat completions tool delta field is not a string",
        )
    })?;
    if let Value::String(assembled) = target {
        assembled.push_str(fragment);
    }
    Ok(())
}

pub(super) fn response_usage(payload: &Value) -> (Usage, Option<u64>) {
    let usage = &payload["usage"];
    let last_request_input = usage["prompt_tokens"]
        .as_u64()
        .or_else(|| usage["input_tokens"].as_u64())
        .or_else(|| {
            Some(
                usage["prompt_cache_hit_tokens"]
                    .as_u64()?
                    .saturating_add(usage["prompt_cache_miss_tokens"].as_u64()?),
            )
        });
    let usage = Usage {
        input_tokens: last_request_input.unwrap_or_default(),
        output_tokens: usage["completion_tokens"]
            .as_u64()
            .or_else(|| usage["output_tokens"].as_u64())
            .unwrap_or_default(),
        cache_read_tokens: usage["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .or_else(|| usage["input_tokens_details"]["cached_tokens"].as_u64())
            .or_else(|| usage["prompt_cache_hit_tokens"].as_u64())
            .or_else(|| usage["cache_read_input_tokens"].as_u64())
            .unwrap_or(0),
        cache_creation_tokens: usage["prompt_tokens_details"]["cache_creation_tokens"]
            .as_u64()
            .or_else(|| usage["input_tokens_details"]["cache_creation_tokens"].as_u64())
            .or_else(|| usage["cache_creation_input_tokens"].as_u64())
            .unwrap_or(0),
        reasoning_tokens: usage["completion_tokens_details"]["reasoning_tokens"]
            .as_u64()
            .or_else(|| usage["output_tokens_details"]["reasoning_tokens"].as_u64())
            .or_else(|| usage["reasoning_tokens"].as_u64()),
    };
    (usage, last_request_input)
}

impl ChatCompletionsProvider {
    /// The exact JSON body `round` posts for `request`. Pure: no network and no
    /// credentials, so `wire_shape` hashes the same bytes that are sent.
    fn round_body(&self, request: &ModelRequest) -> Result<Value> {
        let mut body = self
            .wire_input(request)
            .map_err(|error| classify(ProviderErrorKind::Fatal, error))?;
        body["model"] = json!(request.model);
        body["stream"] = json!(true);
        body["stream_options"] = json!({"include_usage":true});
        // No output limit unless the role sets one; a length stop is continued.
        if let Some(limit) = request.max_output_tokens {
            body["max_tokens"] = json!(limit);
        }
        for (name, value) in self.reasoning_fields.as_object().unwrap() {
            body[name] = value.clone();
        }
        let configured_effort = self.reasoning_fields["reasoning_effort"]
            .as_str()
            .map(super::parse_reasoning_effort)
            .transpose()?;
        let effort = request
            .reasoning_effort
            .or(configured_effort)
            .unwrap_or_default();
        body["reasoning_effort"] = json!(effort.as_str());
        Ok(body)
    }

    /// One request of a reply. Also reports whether it stopped on the output limit.
    async fn round(
        &self,
        request: &ModelRequest,
        on_text: &StreamCallback,
        cancellation: &CancellationToken,
    ) -> Result<(ModelResponse, bool)> {
        let body = self.round_body(request)?;
        let session = request
            .messages
            .first()
            .map(|message| message.session_id.as_str())
            .unwrap_or(&request.session_id);
        let mut meter = super::PhysicalRequest::new("chat-completions", &request.model);
        let result: Result<(ModelResponse, bool)> = async {
            let response = self
                .post(&body, self.opencode_route.then_some(session), cancellation)
                .await?;
            let streamed = stream::is_event_stream(&response);
            let payload = if streamed {
                Self::read_stream(response, on_text.clone(), cancellation).await?
            } else {
                let bytes = stream::read_body(response, cancellation).await?;
                serde_json::from_slice(&bytes).map_err(|_| {
                    classify(
                        ProviderErrorKind::Fatal,
                        anyhow!("Chat completions response is not JSON"),
                    )
                })?
            };
            let (usage, last_request_input) = response_usage(&payload);
            let length = payload["choices"][0]["finish_reason"] == "length";
            let parse = (|| -> Result<ModelResponse> {
                let message = &payload["choices"][0]["message"];
                if !payload["error"].is_null() {
                    if let Some(reason) =
                        super::http_error::terminal_reason(payload.to_string().as_bytes())
                    {
                        return Err(super::terminal_quota_failure(
                            reason,
                            "Chat completions response rejected".into(),
                        ));
                    }
                    return Err(classify(
                        chat_error_kind(&payload["error"]),
                        anyhow!("Chat completions response reported an error"),
                    ));
                }
                if payload["choices"][0]["finish_reason"] == "error" {
                    // Native reasons are protocol codes; do not echo arbitrary
                    // provider diagnostics or user content into the failure.
                    let native = payload["choices"][0]["native_finish_reason"]
                        .as_str()
                        .filter(|reason| {
                            !reason.is_empty()
                                && reason.len() <= 128
                                && reason.bytes().all(|byte| {
                                    byte.is_ascii_alphanumeric()
                                        || matches!(byte, b'_' | b'-' | b'.')
                                })
                        });
                    return Err(anyhow!(
                        "Chat completions response ended with error{}",
                        native
                            .map(|reason| format!(" ({reason})"))
                            .unwrap_or_default()
                    ));
                }
                let mut calls = Vec::new();
                let mut ids = HashSet::new();
                ensure!(
                    message["tool_calls"].is_null() || message["tool_calls"].is_array(),
                    "Chat completions tool_calls must be an array"
                );
                if let Some(wire_calls) = message["tool_calls"].as_array() {
                    for call in wire_calls {
                        let id = call["id"].as_str().filter(|value| !value.is_empty());
                        let name = call["function"]["name"]
                            .as_str()
                            .filter(|value| !value.is_empty());
                        let raw = call["function"]["arguments"].as_str();
                        let arguments =
                            raw.and_then(|value| serde_json::from_str::<Value>(value).ok());
                        // A length stop may split identifiers or JSON. It carries
                        // no executable call; continue the surviving text instead.
                        if length && (id.is_none() || name.is_none() || arguments.is_none()) {
                            continue;
                        }
                        let id =
                            id.ok_or_else(|| anyhow!("Chat completions tool call lacks id"))?;
                        let name =
                            name.ok_or_else(|| anyhow!("Chat completions tool call lacks name"))?;
                        ensure!(
                            tool_wire::accepts_call(name, !request.tools.is_empty()),
                            "Chat completions returned a tool call with tools disabled"
                        );
                        ensure!(
                            call["type"] == "function",
                            "Chat completions tool call is not a function"
                        );
                        ensure!(
                            ids.insert(id.to_owned()),
                            "Chat completions duplicate tool call id"
                        );
                        // A missing wire field breaks the call envelope. Malformed
                        // text still reaches core with its call ID.
                        let raw = raw.ok_or_else(|| {
                            anyhow!("Chat completions tool arguments must be a string")
                        })?;
                        calls.push(ToolCall {
                            id: id.into(),
                            name: name.into(),
                            arguments: arguments.unwrap_or_else(|| tool_wire::raw_arguments(raw)),
                        });
                    }
                }
                ensure!(
                    length
                        || payload["choices"][0]["finish_reason"] != "tool_calls"
                        || !calls.is_empty(),
                    "Chat completions tool_calls stop has no calls"
                );
                ensure!(
                    length
                        || calls.is_empty()
                        || payload["choices"][0]["finish_reason"] == "tool_calls",
                    "Chat completions tool calls have an inconsistent finish_reason"
                );
                // A length stop can end before any visible text; that partial is still a reply.
                let text = match message["content"].as_str() {
                    Some(text) => text.to_owned(),
                    None if length || !calls.is_empty() => String::new(),
                    None => {
                        return Err(classify(
                            ProviderErrorKind::Fatal,
                            anyhow!("Chat completions response has no text"),
                        ));
                    }
                };
                let thinking = message["reasoning_content"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                if !streamed && !thinking.is_empty() {
                    on_text(StreamEvent::ThinkingDelta(&thinking));
                }
                if !streamed && !text.is_empty() {
                    on_text(StreamEvent::TextDelta(&text));
                }
                Ok(ModelResponse {
                    request_usages: vec![usage.clone()],
                    text,
                    thinking,
                    tool_calls: calls,
                    last_request_usage: Some(bubblepaw_core::LastRequestUsage {
                        input_tokens: last_request_input,
                        output_tokens: payload["usage"]["completion_tokens"]
                            .as_u64()
                            .or_else(|| payload["usage"]["output_tokens"].as_u64()),
                    }),
                    usage: Some(usage.clone()),
                    provider_state: Some(bubblepaw_core::with_provider_state_origin(
                        chat_state(&payload),
                        "chat-completions",
                        Some(&request.model),
                        "native",
                    )?),
                    ..super::reply_facts::chat(&payload)
                })
            })();
            parse.map(|response| (response, length)).map_err(|error| {
                super::decisions::metered_model_error(
                    classify(ProviderErrorKind::Fatal, error),
                    payload["usage"].is_object().then_some(usage),
                )
            })
        }
        .await;
        meter.finish_borrowed_result(result.as_ref().map(|(response, ..)| response));
        result
    }
}

#[async_trait]
impl Provider for ChatCompletionsProvider {
    fn supports_summary_fork(&self, _model: &str) -> bool {
        true
    }
    fn summary_fork_mapping(&self, request: &ModelRequest) -> Result<Option<Value>> {
        self.wire_messages_mapped(request, true).map(|(_, mapping)| Some(json!({
            "protocol":"chat-completions", "position_unit":"zero-based messages array index (opening system is 0)", "sources":mapping,
        })))
    }
    async fn generate_summary_fork(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        tool_wire::generate_summary_fork(self, request, cancellation).await
    }
    fn estimate_input_tokens(
        &self,
        request: &ModelRequest,
        count: &dyn Fn(&Value) -> usize,
    ) -> Result<Option<usize>> {
        self.wire_input(request).map(|input| Some(count(&input)))
    }
    fn wire_shape(&self, request: &ModelRequest) -> Option<bubblepaw_core::WireShape> {
        // The first round of a reply is the request as the main loop sends it.
        let body = self.round_body(request).ok()?;
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
        on_text: StreamCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let (on_text, published) = super::track_text_publication(on_text);
        let mut usage = None::<Usage>;
        let result = async {
            let mut request = request;
            let mut text = String::new();
            let mut continuations = 0;
            let mut thinking = String::new();
            let mut rounds = Vec::new();
            let mut request_usages = Vec::new();
            loop {
                let (round, length) = self.round(&request, &on_text, &cancellation).await?;
                let start = text.len();
                text.push_str(&round.text);
                thinking.push_str(&round.thinking);
                if let Some(mut state) = round.provider_state.clone() {
                    state.as_object_mut().unwrap().remove("adapter");
                    state
                        .as_object_mut()
                        .unwrap()
                        .remove(bubblepaw_core::PROVIDER_STATE_ORIGIN_FIELD);
                    state["text_range"] = json!([start, text.len()]);
                    rounds.push(state);
                }
                if let Some(part) = &round.usage {
                    usage.get_or_insert_with(Usage::default).add(part);
                }
                request_usages.extend(round.request_usages.iter().cloned());
                if continuation::should_continue(
                    length,
                    !round.tool_calls.is_empty(),
                    continuations,
                ) {
                    request = if self
                        .reasoning_replay
                        .get(&request.model)
                        .copied()
                        .unwrap_or_default()
                        == ReasoningReplay::All
                    {
                        continuation::continue_response(&request, &round)
                    } else {
                        continuation::continue_request(&request, &round.text)
                    };
                    continuations += 1;
                    continue;
                }
                if length {
                    continuation::warn_incomplete(
                        "chat-completions",
                        &request.model,
                        continuations,
                    );
                }
                return Ok(ModelResponse {
                    text,
                    thinking,
                    tool_calls: round.tool_calls,
                    last_request_usage: round.last_request_usage,
                    model: round.model,
                    stop_reason: round.stop_reason,
                    request_usages,
                    usage: usage.clone(),
                    provider_state: if rounds.len() == 1 {
                        round.provider_state
                    } else {
                        Some(bubblepaw_core::with_provider_state_origin(
                            json!({"adapter":"chat-completions","rounds":rounds}),
                            "chat-completions",
                            Some(&request.model),
                            "native",
                        )?)
                    },
                    ..Default::default()
                });
            }
        }
        .await;
        result.map_err(|error| {
            super::mark_published(
                super::retain_prior_usage(error, usage.as_ref()),
                published.load(Ordering::Acquire),
            )
        })
    }
}

#[cfg(test)]
#[path = "chat_tests.rs"]
mod prepared_accounting_tests;
