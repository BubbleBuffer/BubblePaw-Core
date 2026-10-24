//! Request-scoped Streamable HTTP, with an explicitly separate legacy session path.
use crate::{McpClientEnv, McpServerSettings};
use anyhow::{Result, anyhow, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bubblepaw_core::CancellationToken;
use reqwest::{
    Client, Method, Response, StatusCode, Url,
    header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{sync::mpsc, task::JoinHandle};

const MODERN_VERSION: &str = "2026-07-28";
const LEGACY_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18"];
const FRAME_LIMIT: usize = 8 * 1024 * 1024;
const SAFE_INTEGER: i64 = 9_007_199_254_740_991;
const CONTROL_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Clone, Copy)]
enum Era {
    Modern,
    Legacy(&'static str),
}
impl Era {
    fn version(self) -> &'static str {
        match self {
            Self::Modern => MODERN_VERSION,
            Self::Legacy(version) => version,
        }
    }
    fn legacy(self) -> bool {
        matches!(self, Self::Legacy(_))
    }
}

#[derive(Clone)]
struct Snapshot {
    era: Era,
    session: Option<HeaderValue>,
    generation: u64,
}
#[derive(Default)]
struct State {
    era: Option<Era>,
    session: Option<HeaderValue>,
    generation: u64,
    first_list: Option<Value>,
    // Retain a known new session for cleanup, but do not let another request
    // use it until notifications/initialized has been accepted.
    pending_initialization: Option<Snapshot>,
}
impl State {
    fn snapshot(&self) -> Result<Snapshot> {
        Ok(Snapshot {
            era: self
                .era
                .ok_or_else(|| anyhow!("MCP HTTP is not initialized"))?,
            session: self.session.clone(),
            generation: self.generation,
        })
    }
}

#[derive(Clone)]
struct ParameterHeader {
    name: HeaderName,
    path: Vec<String>,
    primitive: Primitive,
}
#[derive(Clone, Copy)]
enum Primitive {
    String,
    Integer,
    Boolean,
}

struct Cancellation {
    id: u64,
    snapshot: Snapshot,
}

/// All credentials live only in the connection. No URL or remote error body is
/// used in error diagnostics, and successful values are recursively redacted.
pub(crate) struct HttpConnection {
    client: Client,
    url: Url,
    headers: HeaderMap,
    secrets: Vec<String>,
    state: Mutex<State>,
    initialize_gate: tokio::sync::Mutex<()>,
    next_id: AtomicU64,
    alive: AtomicBool,
    tools_changed: AtomicBool,
    stopped: CancellationToken,
    control_stopped: CancellationToken,
    cancellations: Mutex<Option<mpsc::Sender<Cancellation>>>,
    control_task: Mutex<Option<JoinHandle<()>>>,
    startup_timeout: Duration,
    call_timeout: Duration,
}

impl HttpConnection {
    pub(crate) fn new(
        config: &McpServerSettings,
        allowed_envs: &[String],
        client_env: Option<&McpClientEnv>,
    ) -> Result<Arc<Self>> {
        config.validate(allowed_envs)?;
        let url = Url::parse(config.url.as_deref().unwrap_or_default())
            .map_err(|_| anyhow!("invalid MCP HTTP URL"))?;
        let mut headers = HeaderMap::new();
        let mut secrets = Vec::new();
        for (name, variable) in &config.headers_from {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| anyhow!("invalid MCP HTTP credential header name"))?;
            // A supplied client snapshot is the only source: no host fallback.
            // The missing-variable text is the standalone text, on purpose.
            let missing =
                || anyhow!("required MCP HTTP credential environment variable is missing");
            let secret = match client_env {
                Some(client_env) => client_env
                    .get(variable)
                    .map(str::to_owned)
                    .ok_or_else(missing)?,
                None => std::env::var(variable).map_err(|_| missing())?,
            };
            ensure!(
                !secret.is_empty(),
                "MCP HTTP credential environment variable is empty"
            );
            let mut value = HeaderValue::from_str(&secret)
                .map_err(|_| anyhow!("invalid MCP HTTP credential header value"))?;
            value.set_sensitive(true);
            // Bearer credentials are commonly echoed without the scheme.
            if name == reqwest::header::AUTHORIZATION
                && let Some((_, token)) = secret.split_once(' ')
                && !token.is_empty()
            {
                secrets.push(token.to_owned());
            }
            secrets.push(secret);
            headers
                .try_insert(name, value)
                .map_err(|_| anyhow!("MCP HTTP credential header count exceeds the limit"))?;
        }
        headers
            .try_reserve(8)
            .map_err(|_| anyhow!("MCP HTTP credential header count exceeds the limit"))?;
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        secrets.dedup();
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            // reqwest's default retry policy includes protocol-level retries.
            // A tools/call POST must have exactly one transport attempt.
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| anyhow!("cannot create MCP HTTP client"))?;
        Ok(Arc::new(Self {
            client,
            url,
            headers,
            secrets,
            state: Mutex::default(),
            initialize_gate: tokio::sync::Mutex::new(()),
            next_id: AtomicU64::new(1),
            alive: AtomicBool::new(true),
            tools_changed: AtomicBool::new(false),
            stopped: CancellationToken::new(),
            control_stopped: CancellationToken::new(),
            cancellations: Mutex::default(),
            control_task: Mutex::default(),
            startup_timeout: Duration::from_secs(config.startup_timeout_secs),
            call_timeout: Duration::from_secs(config.call_timeout_secs),
        }))
    }

    pub(crate) fn take_tools_changed(&self) -> bool {
        self.tools_changed.swap(false, Ordering::SeqCst)
    }
    pub(crate) fn mark_tools_changed(&self) {
        self.tools_changed.store(true, Ordering::SeqCst);
    }
    pub(crate) fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst) && !self.stopped.is_cancelled()
    }

    pub(crate) fn redact(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for secret in &self.secrets {
            text = text.replace(secret, "[redacted]");
        }
        text
    }

    fn redact_value(&self, value: &mut Value) {
        match value {
            Value::String(text) => *text = self.redact(text),
            Value::Array(values) => {
                for value in values {
                    self.redact_value(value);
                }
            }
            Value::Object(values) => {
                for (key, mut value) in std::mem::take(values) {
                    self.redact_value(&mut value);
                    values.insert(self.redact(&key), value);
                }
            }
            _ => {}
        }
    }

    /// The first modern tools/list is read-only and doubles as era discovery.
    /// Its response is consumed by the host's first ordinary list request.
    pub(crate) async fn initialize(self: &Arc<Self>) -> Result<()> {
        let initialize = async {
            let _gate = self.initialize_gate.lock().await;
            ensure!(self.alive(), "MCP HTTP host shut down");
            if self.state.lock().unwrap().era.is_some() {
                return Ok(());
            }
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            let modern = Snapshot {
                era: Era::Modern,
                session: None,
                generation: 0,
            };
            match self.exchange(id, "tools/list", json!({}), &modern).await {
                Ok(envelope) => {
                    let result = result(envelope.message, id)?;
                    complete_result(&result)?;
                    ensure!(result["tools"].is_array(), "invalid MCP HTTP tool list");
                    let mut state = self.state.lock().unwrap();
                    state.era = Some(Era::Modern);
                    state.first_list = Some(result);
                    Ok(())
                }
                Err(failure) if failure.legacy_probe_rejection() => {
                    self.initialize_legacy(LEGACY_VERSIONS[0]).await?;
                    self.start_control_task();
                    Ok(())
                }
                Err(failure) if failure.advertised_legacy_version.is_some() => {
                    // This is an explicit mutually supported selection from a
                    // recognized modern error, not era inference from HTTP 400.
                    self.initialize_legacy(failure.advertised_legacy_version.unwrap())
                        .await?;
                    self.start_control_task();
                    Ok(())
                }
                Err(failure) => Err(failure.into_error()),
            }
        };
        tokio::select! {
            biased;
            _ = self.stopped.cancelled() => bail!("MCP HTTP host shut down"),
            answer = tokio::time::timeout(self.startup_timeout, initialize) =>
                answer.map_err(|_| anyhow!("MCP HTTP startup timed out"))?,
        }
    }

    async fn initialize_legacy(&self, offered_version: &'static str) -> Result<()> {
        let previous_pending = self.state.lock().unwrap().pending_initialization.take();
        if let Some(previous_pending) = previous_pending {
            self.delete_session(&previous_pending, CONTROL_TIMEOUT)
                .await;
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let offered = Snapshot {
            era: Era::Legacy(offered_version),
            session: None,
            generation: 0,
        };
        let envelope = self
            .exchange(
                id,
                "initialize",
                json!({
                    "protocolVersion": offered_version,
                    "capabilities": {},
                    "clientInfo": {"name":"bubblepaw", "version":env!("CARGO_PKG_VERSION")},
                }),
                &offered,
            )
            .await
            .map_err(WireFailure::into_error)?;
        let value = result(envelope.message, id)?;
        complete_result(&value)?;
        let negotiated = value["protocolVersion"].as_str().unwrap_or_default();
        let version = LEGACY_VERSIONS
            .iter()
            .copied()
            .find(|version| *version == negotiated)
            .ok_or_else(|| anyhow!("incompatible legacy MCP HTTP protocol version"))?;
        ensure!(
            value["capabilities"]["tools"].is_object(),
            "MCP HTTP server does not support tools"
        );
        let session = validate_session(envelope.session)?;
        let snapshot = {
            let mut state = self.state.lock().unwrap();
            let snapshot = Snapshot {
                era: Era::Legacy(version),
                session,
                generation: state.generation + 1,
            };
            state.pending_initialization = Some(snapshot.clone());
            snapshot
        };
        self.send_notification("notifications/initialized", json!({}), &snapshot)
            .await?;
        let mut state = self.state.lock().unwrap();
        state.era = Some(snapshot.era);
        state.session = snapshot.session;
        state.generation = snapshot.generation;
        state.pending_initialization = None;
        Ok(())
    }

    pub(crate) fn register_tool(&self, _name: &str, schema: &Value) -> Result<()> {
        tool_annotations(schema).map(|_| ())
    }

    pub(crate) async fn call_tool(
        self: &Arc<Self>,
        name: &str,
        input: Value,
        schema: &Value,
    ) -> Result<Value> {
        let annotations = tool_annotations(schema)?;
        self.request_with_annotations(
            "tools/call",
            json!({"name":name,"arguments":input}),
            annotations,
        )
        .await
    }

    pub(crate) async fn request(self: &Arc<Self>, method: &str, params: Value) -> Result<Value> {
        self.request_with_annotations(method, params, Vec::new())
            .await
    }

    async fn request_with_annotations(
        self: &Arc<Self>,
        method: &str,
        params: Value,
        annotations: Vec<ParameterHeader>,
    ) -> Result<Value> {
        ensure!(self.alive(), "MCP HTTP host shut down; outcome unknown");
        ensure!(
            method != "initialize",
            "MCP HTTP initialization is owned by the transport"
        );
        if method == "tools/list" && params.get("cursor").is_none() {
            let cached = self.state.lock().unwrap().first_list.take();
            if let Some(mut cached) = cached {
                self.redact_value(&mut cached);
                return Ok(cached);
            }
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let snapshot = self.state.lock().unwrap().snapshot()?;
        let mut guard = RequestGuard {
            connection: self.clone(),
            id,
            snapshot: snapshot.clone(),
            armed: false,
        };
        let request = async {
            guard.armed = true;
            let first = self
                .exchange_with_annotations(id, method, params.clone(), &snapshot, &annotations)
                .await;
            let envelope = match first {
                Err(failure) if failure.stale_session(&snapshot) => {
                    // This POST was rejected before its effect. Unlike a dropped
                    // response/GET, this is positive evidence permitting one retry.
                    guard.armed = false;
                    let _gate = self.initialize_gate.lock().await;
                    let generation = self.state.lock().unwrap().generation;
                    if generation == snapshot.generation {
                        self.initialize_legacy(snapshot.era.version()).await?;
                    }
                    let refreshed = self.state.lock().unwrap().snapshot()?;
                    drop(_gate);
                    guard.snapshot = refreshed.clone();
                    guard.armed = true;
                    self.exchange_with_annotations(id, method, params, &refreshed, &annotations)
                        .await
                }
                answer => answer,
            }
            .map_err(WireFailure::into_error)?;
            // A final server response ends cancellation ownership, including errors.
            final_response(&envelope.message, id)?;
            guard.armed = false;
            let mut value = result(envelope.message, id)?;
            complete_result(&value)?;
            self.redact_value(&mut value);
            Ok(value)
        };
        tokio::select! {
            biased;
            _ = self.stopped.cancelled() => bail!("MCP HTTP host shut down; outcome unknown"),
            answer = tokio::time::timeout(self.call_timeout, request) =>
                answer.map_err(|_| anyhow!("MCP HTTP request timed out; outcome unknown"))?,
        }
    }

    fn request_headers(
        &self,
        method: &str,
        params: &Value,
        snapshot: &Snapshot,
        annotations: &[ParameterHeader],
    ) -> Result<HeaderMap> {
        let mut headers = self.headers.clone();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        // Legacy initialization offers its version in the body. A version
        // header here can be rejected before a server negotiates that offer.
        if method != "initialize" || !snapshot.era.legacy() {
            headers.insert(
                "mcp-protocol-version",
                HeaderValue::from_static(snapshot.era.version()),
            );
        }
        if let Some(session) = &snapshot.session {
            headers.insert("mcp-session-id", session.clone());
        }
        if !snapshot.era.legacy() {
            headers.insert(
                "mcp-method",
                HeaderValue::from_str(method)
                    .map_err(|_| anyhow!("invalid MCP HTTP method header"))?,
            );
            if matches!(method, "tools/call" | "prompts/get" | "resources/read") {
                let name = params
                    .get(if method == "resources/read" {
                        "uri"
                    } else {
                        "name"
                    })
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("MCP HTTP request is missing its name"))?;
                headers.insert("mcp-name", encode_header(name)?);
                if method == "tools/call" {
                    for annotation in annotations {
                        let mut value = &params["arguments"];
                        for property in &annotation.path {
                            value = value.get(property).unwrap_or(&Value::Null);
                        }
                        if value.is_null() {
                            continue;
                        }
                        let plain = primitive_value(value, annotation.primitive)?;
                        headers
                            .try_insert(annotation.name.clone(), encode_header(&plain)?)
                            .map_err(|_| {
                                anyhow!("MCP HTTP mirrored header count exceeds the limit")
                            })?;
                    }
                }
            }
        }
        Ok(headers)
    }

    fn request_body(
        &self,
        id: u64,
        method: &str,
        mut params: Value,
        snapshot: &Snapshot,
    ) -> Result<Vec<u8>> {
        ensure!(
            params.is_object(),
            "MCP HTTP request parameters must be an object"
        );
        if !snapshot.era.legacy() {
            let object = params.as_object_mut().unwrap();
            let metadata = object.entry("_meta").or_insert_with(|| json!({}));
            let metadata = metadata
                .as_object_mut()
                .ok_or_else(|| anyhow!("MCP HTTP request metadata must be an object"))?;
            metadata.insert(
                "io.modelcontextprotocol/protocolVersion".into(),
                json!(MODERN_VERSION),
            );
            metadata.insert(
                "io.modelcontextprotocol/clientInfo".into(),
                json!({"name":"bubblepaw", "version":env!("CARGO_PKG_VERSION")}),
            );
            metadata.insert(
                "io.modelcontextprotocol/clientCapabilities".into(),
                json!({}),
            );
        }
        bounded_json(&json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}))
    }

    async fn exchange(
        &self,
        id: u64,
        method: &str,
        params: Value,
        snapshot: &Snapshot,
    ) -> std::result::Result<Envelope, WireFailure> {
        self.exchange_with_annotations(id, method, params, snapshot, &[])
            .await
    }

    async fn exchange_with_annotations(
        &self,
        id: u64,
        method: &str,
        params: Value,
        snapshot: &Snapshot,
        annotations: &[ParameterHeader],
    ) -> std::result::Result<Envelope, WireFailure> {
        let body = self
            .request_body(id, method, params.clone(), snapshot)
            .map_err(WireFailure::local)?;
        let headers = self
            .request_headers(method, &params, snapshot, annotations)
            .map_err(WireFailure::local)?;
        let response = self
            .client
            .post(self.url.clone())
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(|_| WireFailure::unknown("MCP HTTP transport failed; outcome unknown"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(http_failure(response).await);
        }
        let session = response.headers().get("mcp-session-id").cloned();
        let content_type = media_type(&response);
        let message = match content_type.as_str() {
            "application/json" => {
                let bytes = read_bounded(response).await.map_err(WireFailure::local)?;
                serde_json::from_slice(&bytes).map_err(|_| {
                    WireFailure::unknown("invalid MCP HTTP JSON response; outcome unknown")
                })?
            }
            "text/event-stream" => {
                return self
                    .read_sse(response, id, snapshot, method == "initialize")
                    .await;
            }
            _ => {
                return Err(WireFailure::unknown(
                    "invalid MCP HTTP response content type; outcome unknown",
                ));
            }
        };
        Ok(Envelope { message, session })
    }

    async fn read_sse(
        &self,
        mut response: Response,
        id: u64,
        snapshot: &Snapshot,
        initializing: bool,
    ) -> std::result::Result<Envelope, WireFailure> {
        let mut parser = SseParser::default();
        let mut session = if initializing {
            validate_session(response.headers().get("mcp-session-id").cloned())
                .map_err(WireFailure::local)?
        } else {
            None
        };
        if initializing && session.is_some() {
            self.remember_pending_session(snapshot, &session);
        }
        loop {
            while let Ok(Some(chunk)) = response.chunk().await {
                for byte in chunk {
                    if let Some(event) = parser.push(byte).map_err(WireFailure::local)? {
                        if event.is_empty() {
                            continue;
                        }
                        let message: Value = serde_json::from_slice(&event).map_err(|_| {
                            WireFailure::unknown("invalid MCP HTTP SSE JSON; outcome unknown")
                        })?;
                        if message["jsonrpc"] != "2.0" {
                            return Err(WireFailure::unknown(
                                "invalid MCP HTTP JSON-RPC version; outcome unknown",
                            ));
                        }
                        if message.get("method").is_some() {
                            if let Some(server_id) = message.get("id") {
                                if !snapshot.era.legacy() {
                                    return Err(WireFailure::unknown(
                                        "unsupported MCP HTTP server request; outcome unknown",
                                    ));
                                }
                                let body = json!({"jsonrpc":"2.0","id":server_id,"error":{"code":-32601,"message":"Method not supported"}});
                                let mut control = snapshot.clone();
                                if initializing {
                                    control.session = session.clone().or(control.session);
                                }
                                self.send_control(body, &control, !initializing)
                                    .await
                                    .map_err(|error| {
                                        WireFailure::unknown(&format!("{error}; outcome unknown"))
                                    })?;
                            }
                            if message["method"] == "notifications/tools/list_changed"
                                && message.get("id").is_none()
                            {
                                self.tools_changed.store(true, Ordering::SeqCst);
                            }
                            // Progress and other notifications are transient.
                            continue;
                        }
                        if message["id"].as_u64() != Some(id) {
                            return Err(WireFailure::unknown(
                                "invalid MCP HTTP response id; outcome unknown",
                            ));
                        }
                        return Ok(Envelope { message, session });
                    }
                }
            }
            // Modern streams are not resumable. Closing them is cancellation.
            if !snapshot.era.legacy() {
                return Err(WireFailure::unknown(
                    "MCP HTTP response stream ended; outcome unknown",
                ));
            }
            let cursor = parser
                .last_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    WireFailure::unknown(
                        "MCP HTTP response stream ended without a resume cursor; outcome unknown",
                    )
                })?;
            let cursor = HeaderValue::from_str(cursor).map_err(|_| {
                WireFailure::unknown("invalid MCP HTTP resume cursor; outcome unknown")
            })?;
            tokio::time::sleep(parser.retry).await;
            let mut headers = self.headers.clone();
            headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
            // The InitializeResult may itself arrive on this resumed stream;
            // until then the selected legacy version is not known.
            if !initializing {
                headers.insert(
                    "mcp-protocol-version",
                    HeaderValue::from_static(snapshot.era.version()),
                );
            }
            headers.insert("last-event-id", cursor);
            if let Some(session) = session.as_ref().or(snapshot.session.as_ref()) {
                headers.insert("mcp-session-id", session.clone());
            }
            response = self
                .client
                .get(self.url.clone())
                .headers(headers)
                .send()
                .await
                .map_err(|_| {
                    WireFailure::unknown("MCP HTTP stream resume failed; outcome unknown")
                })?;
            if !response.status().is_success() {
                // The originating POST was already accepted. A GET 404 cannot
                // establish that its tool effect was rejected, so never replay it.
                let failure = http_failure(response).await;
                return Err(WireFailure::unknown(match failure.status {
                    Some(StatusCode::UNAUTHORIZED) => {
                        "MCP HTTP authentication required; OAuth is not supported; configure env bearer credentials; outcome unknown"
                    }
                    Some(StatusCode::FORBIDDEN) => {
                        "MCP HTTP authentication forbidden; outcome unknown"
                    }
                    _ => "MCP HTTP stream resume rejected; outcome unknown",
                }));
            }
            if media_type(&response) != "text/event-stream" {
                return Err(WireFailure::unknown(
                    "invalid MCP HTTP resumed stream content type; outcome unknown",
                ));
            }
            if initializing && response.headers().contains_key("mcp-session-id") {
                session = validate_session(response.headers().get("mcp-session-id").cloned())
                    .map_err(WireFailure::local)?;
                self.remember_pending_session(snapshot, &session);
            }
            parser.reset_frame();
        }
    }

    fn remember_pending_session(&self, snapshot: &Snapshot, session: &Option<HeaderValue>) {
        let mut state = self.state.lock().unwrap();
        state.pending_initialization = Some(Snapshot {
            era: snapshot.era,
            session: session.clone(),
            generation: state.generation + 1,
        });
    }

    async fn send_notification(
        &self,
        method: &str,
        params: Value,
        snapshot: &Snapshot,
    ) -> Result<()> {
        self.send_control(
            json!({"jsonrpc":"2.0", "method":method, "params":params}),
            snapshot,
            true,
        )
        .await
    }

    async fn send_control(
        &self,
        message: Value,
        snapshot: &Snapshot,
        negotiated: bool,
    ) -> Result<()> {
        let body = bounded_json(&message)?;
        let mut headers = self.headers.clone();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if negotiated {
            headers.insert(
                "mcp-protocol-version",
                HeaderValue::from_static(snapshot.era.version()),
            );
        }
        if let Some(session) = &snapshot.session {
            headers.insert("mcp-session-id", session.clone());
        }
        let response = self
            .client
            .post(self.url.clone())
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(|_| anyhow!("MCP HTTP control request failed"))?;
        if response.status() != StatusCode::ACCEPTED {
            return Err(http_failure(response).await.into_error());
        }
        Ok(())
    }

    fn start_control_task(self: &Arc<Self>) {
        let mut slot = self.control_task.lock().unwrap();
        if slot.is_some() {
            return;
        }
        let (sender, mut receiver) = mpsc::channel::<Cancellation>(64);
        *self.cancellations.lock().unwrap() = Some(sender);
        let connection = Arc::downgrade(self);
        let stopped = self.control_stopped.clone();
        *slot = Some(tokio::spawn(async move {
            loop {
                let cancellation = tokio::select! {
                    biased;
                    _ = stopped.cancelled() => break,
                    cancellation = receiver.recv() => match cancellation {
                        Some(cancellation) => cancellation,
                        None => break,
                    },
                };
                let Some(connection) = connection.upgrade() else {
                    break;
                };
                let notification = connection.send_notification(
                    "notifications/cancelled",
                    json!({"requestId":cancellation.id, "reason":"Request cancelled"}),
                    &cancellation.snapshot,
                );
                tokio::select! {
                    biased;
                    _ = stopped.cancelled() => break,
                    _ = tokio::time::timeout(CONTROL_TIMEOUT, notification) => {},
                }
            }
        }));
    }

    pub(crate) fn stop(&self, _grace: Duration) {
        self.alive.store(false, Ordering::SeqCst);
        self.stopped.cancel();
        self.control_stopped.cancel();
        self.cancellations.lock().unwrap().take();
    }

    pub(crate) async fn shutdown(&self, grace: Duration) {
        let began = Instant::now();
        self.stop(grace);
        let control_task = self.control_task.lock().unwrap().take();
        if let Some(task) = control_task {
            let mut task = OwnedTask(task);
            if tokio::time::timeout(grace, &mut task.0).await.is_err() {
                task.0.abort();
                let _ = (&mut task.0).await;
            }
        }
        let snapshots = {
            let mut state = self.state.lock().unwrap();
            let mut snapshots = Vec::new();
            if let Ok(snapshot) = state.snapshot() {
                snapshots.push(snapshot);
            }
            if let Some(pending) = state.pending_initialization.take() {
                snapshots.push(pending);
            }
            state.session = None;
            snapshots
        };
        for snapshot in snapshots {
            self.delete_session(&snapshot, grace.saturating_sub(began.elapsed()))
                .await;
        }
    }

    async fn delete_session(&self, snapshot: &Snapshot, grace: Duration) {
        if snapshot.era.legacy()
            && let Some(session) = &snapshot.session
            && !grace.is_zero()
        {
            let mut headers = self.headers.clone();
            headers.insert(
                "mcp-protocol-version",
                HeaderValue::from_static(snapshot.era.version()),
            );
            headers.insert("mcp-session-id", session.clone());
            // DELETE is owned by this future and bounded by the caller's grace.
            let _ = tokio::time::timeout(
                grace,
                self.client
                    .request(Method::DELETE, self.url.clone())
                    .headers(headers)
                    .send(),
            )
            .await;
        }
    }
}

/// A cancelled shutdown future still aborts the owned control task.
struct OwnedTask(JoinHandle<()>);
impl Drop for OwnedTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Drop for HttpConnection {
    fn drop(&mut self) {
        self.stop(Duration::ZERO);
        if let Some(task) = self.control_task.lock().unwrap().take() {
            task.abort();
        }
    }
}

struct RequestGuard {
    connection: Arc<HttpConnection>,
    id: u64,
    snapshot: Snapshot,
    armed: bool,
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        if self.armed
            && self.snapshot.era.legacy()
            && let Some(sender) = self.connection.cancellations.lock().unwrap().as_ref()
        {
            // Synchronous, bounded queueing; the connection owns and joins the
            // notification task. No detached task is created by a dropped call.
            let _ = sender.try_send(Cancellation {
                id: self.id,
                snapshot: self.snapshot.clone(),
            });
        }
    }
}

struct Envelope {
    message: Value,
    session: Option<HeaderValue>,
}

fn validate_session(session: Option<HeaderValue>) -> Result<Option<HeaderValue>> {
    session
        .map(|mut session| {
            ensure!(
                !session.is_empty()
                    && session
                        .as_bytes()
                        .iter()
                        .all(|byte| (0x21..=0x7e).contains(byte)),
                "invalid MCP HTTP session identifier; outcome unknown"
            );
            session.set_sensitive(true);
            Ok(session)
        })
        .transpose()
}

struct WireFailure {
    status: Option<StatusCode>,
    modern_error: bool,
    // Only locally known static versions survive inspection of remote data.
    advertised_legacy_version: Option<&'static str>,
    message: String,
}
impl WireFailure {
    fn unknown(message: &str) -> Self {
        Self {
            status: None,
            modern_error: false,
            advertised_legacy_version: None,
            message: message.into(),
        }
    }
    fn local(error: anyhow::Error) -> Self {
        Self::unknown(&error.to_string())
    }
    fn legacy_probe_rejection(&self) -> bool {
        self.status == Some(StatusCode::BAD_REQUEST) && !self.modern_error
    }
    fn stale_session(&self, snapshot: &Snapshot) -> bool {
        self.status == Some(StatusCode::NOT_FOUND)
            && snapshot.era.legacy()
            && snapshot.session.is_some()
    }
    fn into_error(self) -> anyhow::Error {
        anyhow!(self.message)
    }
}

async fn http_failure(response: Response) -> WireFailure {
    let status = response.status();
    // Only era detection needs the body. In particular, do not let a hanging
    // unauthorized response hide the explicit credential diagnostic.
    let body = if status == StatusCode::BAD_REQUEST {
        let bytes = match read_bounded(response).await {
            Ok(bytes) => bytes,
            Err(error) => return WireFailure::local(error),
        };
        serde_json::from_slice::<Value>(&bytes).ok()
    } else {
        None
    };
    let modern_code = body
        .as_ref()
        .filter(|body| body["jsonrpc"] == "2.0")
        .and_then(|body| body["error"]["code"].as_i64())
        .filter(|code| matches!(code, -32022..=-32020));
    let supported = body
        .as_ref()
        .and_then(|body| body["error"]["data"]["supported"].as_array());
    let inconsistent_version = modern_code == Some(-32022)
        && supported.is_some_and(|versions| {
            versions
                .iter()
                .any(|version| version.as_str() == Some(MODERN_VERSION))
        });
    let advertised_legacy_version = if modern_code == Some(-32022) && !inconsistent_version {
        LEGACY_VERSIONS.iter().copied().find(|known| {
            supported.is_some_and(|versions| {
                versions
                    .iter()
                    .any(|version| version.as_str() == Some(*known))
            })
        })
    } else {
        None
    };
    let message = match (status, modern_code) {
        (StatusCode::UNAUTHORIZED, _) => {
            "MCP HTTP authentication required; OAuth is not supported; configure env bearer credentials".into()
        }
        (StatusCode::FORBIDDEN, _) => "MCP HTTP authentication forbidden".into(),
        (_, Some(-32020)) => "MCP HTTP header validation rejected the request".into(),
        (_, Some(-32021)) => "MCP HTTP server requires an unsupported client capability".into(),
        (_, Some(-32022)) if inconsistent_version =>
            "inconsistent MCP HTTP protocol version rejection".into(),
        (_, Some(-32022)) => "incompatible modern MCP HTTP protocol version".into(),
        _ => format!(
            "MCP HTTP request rejected (status {}); outcome unknown",
            status.as_u16()
        ),
    };
    WireFailure {
        status: Some(status),
        modern_error: modern_code.is_some(),
        advertised_legacy_version,
        message,
    }
}

fn final_response(message: &Value, id: u64) -> Result<()> {
    ensure!(
        message["jsonrpc"] == "2.0",
        "invalid MCP HTTP JSON-RPC version; outcome unknown"
    );
    ensure!(
        message["id"].as_u64() == Some(id),
        "invalid MCP HTTP response id; outcome unknown"
    );
    ensure!(
        message.get("method").is_none(),
        "invalid MCP HTTP response envelope; outcome unknown"
    );
    ensure!(
        matches!(
            (message.get("result"), message.get("error")),
            (Some(Value::Object(_)), None) | (None, Some(Value::Object(_)))
        ),
        "invalid MCP HTTP response envelope; outcome unknown"
    );
    if let Some(error) = message.get("error") {
        ensure!(
            error["code"].as_i64().is_some() && error["message"].is_string(),
            "invalid MCP HTTP error envelope; outcome unknown"
        );
    }
    Ok(())
}

fn result(message: Value, id: u64) -> Result<Value> {
    final_response(&message, id)?;
    match (message.get("result"), message.get("error")) {
        (Some(result), None) if result.is_object() => Ok(result.clone()),
        (None, Some(error)) => {
            let code = error["code"].as_i64().unwrap_or(-32000);
            // Remote text may contain URLs, credentials or unbounded diagnostics.
            bail!("MCP HTTP JSON-RPC error {code}")
        }
        _ => bail!("invalid MCP HTTP response envelope; outcome unknown"),
    }
}

fn complete_result(result: &Value) -> Result<()> {
    match result.get("resultType").and_then(Value::as_str) {
        None if result.get("resultType").is_none() => Ok(()),
        Some("complete") => Ok(()),
        Some("input_required") => {
            bail!("MCP HTTP input-required results are unsupported; outcome unknown")
        }
        _ => bail!("invalid MCP HTTP result type; outcome unknown"),
    }
}

fn media_type(response: &Response) -> String {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

async fn read_bounded(mut response: Response) -> Result<Vec<u8>> {
    ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= FRAME_LIMIT as u64),
        "MCP HTTP response exceeds the frame limit; outcome unknown"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow!("MCP HTTP response read failed; outcome unknown"))?
    {
        ensure!(
            bytes.len().saturating_add(chunk.len()) <= FRAME_LIMIT,
            "MCP HTTP response exceeds the frame limit; outcome unknown"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Serialize into a bounded writer, rather than allocate an arbitrarily large
/// encoded body and only then check its size.
fn bounded_json(value: &Value) -> Result<Vec<u8>> {
    struct Writer(Vec<u8>);
    impl std::io::Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.0.len().saturating_add(bytes.len()) > FRAME_LIMIT {
                return Err(std::io::Error::other("frame limit"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Writer(Vec::new());
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| anyhow!("MCP HTTP request exceeds the frame limit"))?;
    Ok(writer.0)
}

fn encode_header(value: &str) -> Result<HeaderValue> {
    let plain = value
        .as_bytes()
        .iter()
        .all(|byte| matches!(byte, b'\t' | 0x20..=0x7e))
        && !value.starts_with([' ', '\t'])
        && !value.ends_with([' ', '\t'])
        && !(value.starts_with("=?base64?") && value.ends_with("?="));
    let encoded;
    let value = if plain {
        value
    } else {
        encoded = format!("=?base64?{}?=", STANDARD.encode(value.as_bytes()));
        &encoded
    };
    HeaderValue::from_str(value).map_err(|_| anyhow!("invalid MCP HTTP mirrored header value"))
}

fn primitive_value(value: &Value, primitive: Primitive) -> Result<String> {
    match primitive {
        Primitive::String => value.as_str().map(str::to_owned),
        Primitive::Boolean => value.as_bool().map(|value| value.to_string()),
        Primitive::Integer => {
            if let Some(value) = value
                .as_i64()
                .filter(|value| (-SAFE_INTEGER..=SAFE_INTEGER).contains(value))
            {
                Some(value.to_string())
            } else if let Some(value) = value.as_u64().filter(|value| *value <= SAFE_INTEGER as u64)
            {
                Some(value.to_string())
            } else {
                // JSON Schema's integer also accepts 42.0; mirror its decimal
                // integer value only inside JavaScript's exact safe range.
                value
                    .as_f64()
                    .filter(|value| {
                        value.is_finite()
                            && value.fract() == 0.0
                            && value.abs() <= SAFE_INTEGER as f64
                    })
                    .map(|value| format!("{value:.0}"))
            }
        }
    }
    .ok_or_else(|| anyhow!("invalid MCP HTTP annotated parameter type or integer range"))
}

/// Walk schema nodes along `properties` only. Other subtrees are scanned for
/// annotations and rejected, without mistaking property names for keywords.
fn inspect_schema(
    schema: &Value,
    path: &mut Vec<String>,
    reachable: bool,
    names: &mut BTreeSet<String>,
    headers: &mut Vec<ParameterHeader>,
) -> Result<()> {
    let Some(object) = schema.as_object() else {
        return Ok(());
    };
    if let Some(annotation) = object.get("x-mcp-header") {
        ensure!(
            reachable && !path.is_empty(),
            "x-mcp-header must be reachable through properties only"
        );
        let suffix = annotation
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("x-mcp-header must be a nonempty header token"))?;
        HeaderName::from_bytes(suffix.as_bytes())
            .map_err(|_| anyhow!("x-mcp-header must be a valid header token"))?;
        ensure!(
            names.insert(suffix.to_ascii_lowercase()),
            "duplicate x-mcp-header name"
        );
        let primitive = match object.get("type").and_then(Value::as_str) {
            Some("string") => Primitive::String,
            Some("integer") => Primitive::Integer,
            Some("boolean") => Primitive::Boolean,
            _ => bail!("x-mcp-header requires a string, integer or boolean parameter"),
        };
        headers.push(ParameterHeader {
            name: HeaderName::from_bytes(format!("Mcp-Param-{suffix}").as_bytes())
                .map_err(|_| anyhow!("invalid x-mcp-header name"))?,
            path: path.clone(),
            primitive,
        });
    }
    for (keyword, value) in object {
        if keyword == "properties" {
            if let Some(properties) = value.as_object() {
                for (name, property) in properties {
                    path.push(name.clone());
                    inspect_schema(property, path, true, names, headers)?;
                    path.pop();
                }
            }
        } else if keyword != "x-mcp-header" && contains_annotation(value) {
            bail!("x-mcp-header appears outside a properties path");
        }
    }
    Ok(())
}

fn contains_annotation(value: &Value) -> bool {
    match value {
        Value::Object(values) => {
            values.contains_key("x-mcp-header") || values.values().any(contains_annotation)
        }
        Value::Array(values) => values.iter().any(contains_annotation),
        _ => false,
    }
}

struct SseParser {
    line: Vec<u8>,
    data: Vec<u8>,
    event_id: Option<String>,
    last_id: Option<String>,
    event_type: String,
    retry: Duration,
    previous_cr: bool,
    first_line: bool,
}
impl Default for SseParser {
    fn default() -> Self {
        Self {
            line: Vec::new(),
            data: Vec::new(),
            event_id: None,
            last_id: None,
            event_type: String::new(),
            retry: Duration::from_secs(1),
            previous_cr: false,
            first_line: true,
        }
    }
}
impl SseParser {
    fn push(&mut self, byte: u8) -> Result<Option<Vec<u8>>> {
        if self.previous_cr && byte == b'\n' {
            self.previous_cr = false;
            return Ok(None);
        }
        self.previous_cr = byte == b'\r';
        if matches!(byte, b'\n' | b'\r') {
            return self.finish_line();
        }
        ensure!(
            self.line
                .len()
                .saturating_add(self.data.len())
                .saturating_add(self.event_type.len())
                .saturating_add(self.event_id.as_ref().map_or(0, String::len))
                .saturating_add(self.last_id.as_ref().map_or(0, String::len))
                < FRAME_LIMIT,
            "MCP HTTP SSE frame exceeds the frame limit; outcome unknown"
        );
        self.line.push(byte);
        Ok(None)
    }
    fn finish_line(&mut self) -> Result<Option<Vec<u8>>> {
        let mut bytes = std::mem::take(&mut self.line);
        if self.first_line {
            self.first_line = false;
            if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
                bytes.drain(..3);
            }
        }
        let line = std::str::from_utf8(&bytes)
            .map_err(|_| anyhow!("invalid MCP HTTP SSE UTF-8; outcome unknown"))?;
        if line.is_empty() {
            if let Some(id) = self.event_id.take() {
                self.last_id = Some(id);
            }
            let mut data = std::mem::take(&mut self.data);
            if data.last() == Some(&b'\n') {
                data.pop();
            }
            let accepted = self.event_type.is_empty() || self.event_type == "message";
            self.event_type.clear();
            return Ok(if accepted { Some(data) } else { None });
        }
        if line.starts_with(':') {
            return Ok(None);
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "data" => {
                ensure!(
                    self.data
                        .len()
                        .saturating_add(value.len())
                        .saturating_add(1)
                        <= FRAME_LIMIT,
                    "MCP HTTP SSE frame exceeds the frame limit; outcome unknown"
                );
                self.data.extend_from_slice(value.as_bytes());
                self.data.push(b'\n');
            }
            "id" if !value.contains('\0') => self.event_id = Some(value.to_owned()),
            "event" => self.event_type = value.to_owned(),
            "retry" if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => {
                if let Ok(milliseconds) = value.parse::<u64>() {
                    self.retry = Duration::from_millis(milliseconds);
                }
            }
            _ => {}
        }
        Ok(None)
    }
    fn reset_frame(&mut self) {
        self.line.clear();
        self.data.clear();
        self.event_id = None;
        self.event_type.clear();
        self.previous_cr = false;
        self.first_line = true;
    }
}

fn tool_annotations(schema: &Value) -> Result<Vec<ParameterHeader>> {
    let mut headers = Vec::new();
    inspect_schema(
        schema,
        &mut Vec::new(),
        false,
        &mut BTreeSet::new(),
        &mut headers,
    )?;
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirrored_headers_encode_controls_unicode_whitespace_and_literal_sentinel() {
        for value in ["hello", "", "42", "true", "a\tb"] {
            assert_eq!(encode_header(value).unwrap().to_str().unwrap(), value);
        }
        for value in [
            " padded ",
            "line\nnext",
            "世界",
            "=?base64?literal?=",
            "\ttab",
        ] {
            assert_eq!(
                encode_header(value).unwrap().to_str().unwrap(),
                format!("=?base64?{}?=", STANDARD.encode(value))
            );
        }
    }

    #[test]
    fn annotation_validation_requires_unique_primitive_properties_paths() {
        let valid = json!({"type":"object","properties":{"nested":{"type":"object","properties":{
            "value":{"type":"integer","x-mcp-header":"Value"}}}}});
        let mut headers = Vec::new();
        inspect_schema(
            &valid,
            &mut Vec::new(),
            false,
            &mut BTreeSet::new(),
            &mut headers,
        )
        .unwrap();
        assert_eq!(headers[0].path, ["nested", "value"]);
        for invalid in [
            json!({"type":"string","x-mcp-header":"Root"}),
            json!({"properties":{"value":{"type":"number","x-mcp-header":"Value"}}}),
            json!({"properties":{"a":{"type":"string","x-mcp-header":"value"},"b":{"type":"string","x-mcp-header":"Value"}}}),
            json!({"items":{"properties":{"value":{"type":"string","x-mcp-header":"Value"}}}}),
            json!({"properties":{"value":{"type":"string","x-mcp-header":"bad\r\nname"}}}),
            json!({"$defs":{"value":{"type":"string","x-mcp-header":"Value"}}}),
        ] {
            assert!(
                inspect_schema(
                    &invalid,
                    &mut Vec::new(),
                    false,
                    &mut BTreeSet::new(),
                    &mut Vec::new()
                )
                .is_err()
            );
        }
        assert!(primitive_value(&json!(SAFE_INTEGER + 1), Primitive::Integer).is_err());
        assert_eq!(
            primitive_value(&json!(42.0), Primitive::Integer).unwrap(),
            "42"
        );
    }

    #[test]
    fn sse_parser_handles_split_crlf_multiline_comments_cursor_and_retry() {
        let mut parser = SseParser::default();
        let mut events = Vec::new();
        for byte in b"\xef\xbb\xbf:keepalive\r\nid: cursor\r\nretry: 27\r\ndata:\r\n\r\ndata: {\r\ndata: \"ok\":true}\r\n\r\n" {
            if let Some(event) = parser.push(*byte).unwrap() { events.push(event); }
        }
        assert_eq!(parser.last_id.as_deref(), Some("cursor"));
        assert_eq!(parser.retry, Duration::from_millis(27));
        assert_eq!(events, [Vec::new(), b"{\n\"ok\":true}".to_vec()]);
        let mut parser = SseParser::default();
        for _ in 0..FRAME_LIMIT {
            parser.push(b'x').unwrap();
        }
        assert!(parser.push(b'x').is_err());
    }
}
