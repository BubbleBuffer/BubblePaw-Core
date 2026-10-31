//! Optional, disposable observations. Sinks must enqueue without blocking execution.
//! This stream is never an effect ledger or a source of conversation history.
use crate::cache_accounting::{CacheAccounting, RequestKind, WireShape, request_components};
use crate::{ModelRequest, ModelResponse, Usage, now};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObservationMode {
    #[default]
    Metadata,
    Contents,
}

/// Implementations must return promptly, dropping records if their bounded queue is full.
pub trait ObservationSink: Send + Sync {
    fn record(&self, record: ObservationRecord);
}

#[derive(Clone)]
pub struct Observation {
    pub sink: Arc<dyn ObservationSink>,
    pub mode: ObservationMode,
    pub max_payload_bytes: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ObservationScope {
    pub session_id: String,
    pub run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invocation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObservationRecord {
    pub created_at: String,
    pub kind: String,
    #[serde(flatten)]
    pub scope: ObservationScope,
    pub metrics: Value,
    /// A bounded textual JSON preview, only present in explicitly enabled contents mode.
    /// Truncated previews are not independently parseable; the enclosing record always is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
    pub payload_truncated: bool,
}

fn merge(target: &mut Value, extra: Value) {
    if let (Some(target), Value::Object(extra)) = (target.as_object_mut(), extra) {
        target.extend(extra);
    }
}

pub fn fingerprint(bytes: &[u8]) -> String {
    crate::hex_lower(Sha256::digest(bytes))
}

/// Provider continuation state and structured credentials are excluded in every mode.
fn scrub(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|key, _| {
                let key = key.to_ascii_lowercase().replace(['_', '-'], "");
                !key.ends_with("apikey")
                    && !key.ends_with("password")
                    && !key.ends_with("secret")
                    && !matches!(
                        key.as_str(),
                        "providerstate"
                            | "authorization"
                            | "headers"
                            | "apikey"
                            | "accesstoken"
                            | "refreshtoken"
                            | "password"
                            | "credentials"
                            | "secret"
                            | "clientsecret"
                            | "token"
                            | "cookie"
                            | "setcookie"
                    )
            });
            for value in map.values_mut() {
                scrub(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                scrub(value);
            }
        }
        _ => {}
    }
}

impl Observation {
    /// Observe the already-built context without exposing prompt content.
    pub fn context_built(&self, scope: ObservationScope, record: &crate::ContextBuild) {
        self.emit(scope, "context_built", record.metrics(), None);
    }
    pub(crate) fn emit(
        &self,
        scope: ObservationScope,
        kind: &str,
        metrics: Value,
        payload: Option<Value>,
    ) {
        let (payload, payload_truncated) = if self.mode == ObservationMode::Contents {
            match payload {
                Some(mut value) => {
                    scrub(&mut value);
                    let raw = value.to_string();
                    let mut end = self.max_payload_bytes.min(raw.len());
                    while !raw.is_char_boundary(end) {
                        end -= 1;
                    }
                    let truncated = end < raw.len();
                    (Some(raw[..end].to_string()), truncated)
                }
                None => (None, false),
            }
        } else {
            (None, false)
        };
        self.sink.record(ObservationRecord {
            created_at: now(),
            kind: kind.into(),
            scope,
            metrics,
            payload,
            payload_truncated,
        });
    }
    /// `cache` is the host's ledger (`Store::cache_accounting`), traced or not.
    pub(crate) fn model_start(
        &self,
        scope: ObservationScope,
        ledger: &CacheAccounting,
        request: &ModelRequest,
        wire: Option<WireShape>,
    ) {
        // Capturable content explicitly excludes provider continuation state.
        let messages: Vec<_> = request.messages.iter().map(|m| json!({"id":m.id,"role":m.role,"content":m.content,"tool_calls":m.tool_calls,"tool_call_id":m.tool_call_id,"is_error":m.is_error})).collect();
        let safe = json!({"model":request.model,"system_prompt":request.system_prompt,"messages":messages,"tools":request.tools,"max_output_tokens":request.max_output_tokens});
        let bytes = safe.to_string();
        let definitions = serde_json::to_vec(&request.tools).unwrap_or_default();
        let prepared_bytes = serde_json::to_vec(request).map_or(0, |v| v.len());
        let mut metrics = json!({"model":request.model,"message_count":request.messages.len(),"tool_count":request.tools.len(),"tool_definitions_bytes":definitions.len(),"tool_definitions_fingerprint":fingerprint(&definitions),"prepared_bytes":prepared_bytes,"capturable_bytes":bytes.len(),"system_prompt_bytes":request.system_prompt.len(),"fingerprint":fingerprint(bytes.as_bytes())});
        if let (Some(shape), Some(id)) = (wire, &scope.model_request_id) {
            let cache = ledger.start(
                id,
                &scope.session_id,
                RequestKind::Main,
                shape,
                request_components(request),
            );
            merge(&mut metrics, cache);
        }
        self.emit(scope, "model_started", metrics, Some(safe));
    }
    pub(crate) fn model_finish(
        &self,
        scope: ObservationScope,
        ledger: &CacheAccounting,
        response: &ModelResponse,
        elapsed_ms: u64,
        first_text_ms: Option<u64>,
    ) {
        let output = json!({"text":response.text,"tool_calls":response.tool_calls});
        let mut metadata = json!({"elapsed_ms":elapsed_ms,"first_text_ms":first_text_ms,"text_bytes":response.text.len(),"tool_call_count":response.tool_calls.len(),"usage":response.usage,"fingerprint":fingerprint(output.to_string().as_bytes())});
        // Model fallback routes and their classified reasons (no raw provider text).
        if let Some(state) = &response.provider_state {
            for (key, name) in [
                ("_bubblepaw_fallback_from", "fallback_from"),
                ("_bubblepaw_fallback_reasons", "fallback_reasons"),
            ] {
                if let Some(value) = state.get(key) {
                    metadata[name] = value.clone();
                }
            }
        }
        let mut bust = None;
        if let Some(id) = &scope.model_request_id {
            let state = response.provider_state.as_ref();
            let selected = state
                .and_then(|state| state.get("_bubblepaw_selected_model"))
                .and_then(Value::as_str);
            let single_round = match (&response.last_request_usage, &response.usage) {
                (Some(last), Some(usage)) => last
                    .input_tokens
                    .is_none_or(|input| input == usage.input_tokens),
                _ => true,
            };
            if let Some(outcome) =
                ledger.finish(id, response.usage.as_ref(), selected, single_round)
            {
                merge(&mut metadata, outcome.metrics());
                bust = outcome.bust_event();
            }
        }
        self.emit(scope.clone(), "model_finished", metadata, Some(output));
        if let Some(bust) = bust {
            self.emit(scope, "cache_bust", bust, None);
        }
    }
    pub(crate) fn failure(
        &self,
        scope: ObservationScope,
        kind: &str,
        elapsed_ms: u64,
        category: &str,
        error: &str,
    ) {
        self.emit(scope, kind, json!({"elapsed_ms":elapsed_ms,"error_category":category,"error_bytes":error.len(),"error_fingerprint":fingerprint(error.as_bytes())}), Some(json!({"error":error})));
    }
    pub(crate) fn run_finished(
        &self,
        scope: ObservationScope,
        status: &str,
        usage: Option<&Usage>,
    ) {
        self.emit(
            scope,
            "run_finished",
            json!({"status":status,"usage":usage}),
            None,
        );
    }
}

pub(crate) struct ToolObservation {
    observer: Observation,
    scope: ObservationScope,
    started: std::time::Instant,
    finished: bool,
    tool: String,
}
impl ToolObservation {
    pub(crate) fn start(
        observer: Observation,
        scope: ObservationScope,
        name: &str,
        input: &Value,
        nested: bool,
        origin: &str,
    ) -> Self {
        let bytes = input.to_string();
        let command = input.get("command");
        let parts = command
            .and_then(|c| c.get("parts"))
            .and_then(Value::as_array);
        let values = command
            .and_then(|c| c.get("values"))
            .and_then(Value::as_array);
        let command_kind = if parts.is_some() && values.is_some() {
            "template"
        } else if command.is_some_and(Value::is_string) {
            "raw_script"
        } else if input.get("argv").is_some_and(Value::is_array) {
            "argv"
        } else {
            "none"
        };
        // This is deliberately not a shell parser. Only literal, bare first tokens are named.
        let bare = |s: &str| {
            s.split_whitespace()
                .next()
                .filter(|p| {
                    p.len() <= 128
                        && p.chars()
                            .all(|c| c.is_ascii_alphanumeric() || "_./\\:-".contains(c))
                })
                .map(|p| p.rsplit(['/', '\\']).next().unwrap_or(p).to_string())
        };
        let script_prefix = command.and_then(Value::as_str).and_then(bare);
        let program = input
            .get("argv")
            .and_then(Value::as_array)
            .and_then(|v| v.first())
            .and_then(Value::as_str)
            .map(|p| p.rsplit(['/', '\\']).next().unwrap_or(p).to_string())
            .or_else(|| {
                parts
                    .and_then(|p| p.first())
                    .and_then(Value::as_str)
                    .and_then(bare)
            });
        observer.emit(scope.clone(), "tool_started", json!({"tool":name,"nested":nested,"origin":origin,"input_bytes":bytes.len(),"input_fingerprint":fingerprint(bytes.as_bytes()),"command_kind":command_kind,"program_name":program,"script_prefix":script_prefix,"template_parts":parts.map(Vec::len),"template_values":values.map(Vec::len),"template_fingerprint":parts.map(|p|fingerprint(json!(p).to_string().as_bytes())),"code_bytes":input.get("code").and_then(Value::as_str).map(str::len),"form_fields":input.as_object().map(|v|v.len()),"path_fingerprint":input.get("path").map(|p|fingerprint(p.to_string().as_bytes()))}), Some(input.clone()));
        Self {
            observer,
            scope,
            started: std::time::Instant::now(),
            finished: false,
            tool: name.into(),
        }
    }
    pub(crate) fn invocation(&mut self, id: &str) {
        self.scope.invocation_id = Some(id.into());
    }
    pub(crate) fn finish(&mut self, result: &anyhow::Result<crate::ToolResult>) {
        self.finished = true;
        match result {
            Ok(result) => {
                let content = serde_json::from_str::<Value>(&result.content)
                    .unwrap_or_else(|_| Value::String(result.content.clone()));
                let process = process_metrics(&self.tool, &content);
                self.observer.emit(self.scope.clone(), "tool_finished", json!({"elapsed_ms":self.started.elapsed().as_millis() as u64,"is_error":result.is_error,"pending":result.pending,"result_bytes":result.content.len(),"result_fingerprint":fingerprint(result.content.as_bytes()),"process":process}), Some(json!({"content":content,"is_error":result.is_error,"pending":result.pending})));
            }
            Err(error) => self.observer.failure(
                self.scope.clone(),
                "tool_failed",
                self.started.elapsed().as_millis() as u64,
                "dispatch_error",
                &error.to_string(),
            ),
        }
    }
}

/// Typed host process observations, never arbitrary strings from stdout/stderr.
fn process_metrics(tool: &str, result: &Value) -> Option<Value> {
    if !matches!(
        tool,
        "exec" | "poll" | "wait" | "cancel" | "process_run" | "process_poll" | "process_cancel"
    ) || (!result.get("handle").is_some_and(Value::is_string)
        && !result.get("process_id").is_some_and(Value::is_string))
    {
        return None;
    }
    let status = result.get("status").and_then(Value::as_str).filter(|s| {
        matches!(
            *s,
            "running" | "exited" | "cancelled" | "timed_out" | "timeout" | "completed"
        )
    });
    let stream = |name: &str| {
        let capture = &result["capture"][name];
        let legacy = &result[name];
        json!({"total_bytes":capture["totalBytes"].as_u64().or_else(||legacy["total_bytes"].as_u64()).or_else(||result["cursor"][name].as_u64()),"gap_count":capture["gaps"].as_array().map(Vec::len),"capture_error":capture["captureError"].as_bool().or_else(||legacy["capture_error"].as_bool()),"returned_bytes":legacy.as_str().map(str::len),"truncated":legacy["truncated"].as_bool()})
    };
    Some(
        json!({"status":status,"exit_code":result["exitCode"].as_i64().or_else(||result["exit_code"].as_i64()),"running":result["running"].as_bool(),"cancelled":result["cancelled"].as_bool().or_else(||status.map(|s|s=="cancelled")),"timed_out":result["timed_out"].as_bool().or_else(||status.map(|s|s=="timed_out")),"wait_timed_out":status.map(|s|s=="timeout"),"stdout":stream("stdout"),"stderr":stream("stderr")}),
    )
}
impl Drop for ToolObservation {
    fn drop(&mut self) {
        if !self.finished {
            self.observer.failure(
                self.scope.clone(),
                "tool_failed",
                self.started.elapsed().as_millis() as u64,
                "cancelled_or_abandoned",
                "Tool future dropped; outcome may be unknown",
            );
        }
    }
}
