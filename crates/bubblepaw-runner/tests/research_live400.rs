//! Actual research-child Messages loop, with offline wire replay and opt-in capture.
//! Live: BP_LIVE_RESEARCH_400=1 OPENCODE_API_KEY=... cargo test -p
//! bubblepaw-runner --test research_live400 live_research_child -- --ignored --nocapture
//! Keys belong in the remote environment; this test never prints/captures headers.
//! Captures: .run/live400/<uuid>/{request-NNN.json,response-NNN.body,response-NNN.status}.
//! The durable .run/live400/request-count ceiling is shared across invocations.

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use bubblepaw_core::{
    CancellationToken, Capabilities, HarnessConfig, ModelRequest, ModelResponse, Provider,
    ReasoningEffort, Role, Store, TextCallback, ToolContext, ToolRegistry,
};
use bubblepaw_runner::{
    agents::AgentTools,
    artifacts::ArtifactStore,
    codemode::CodeMode,
    net::{SearchBackend, WebConfig},
    provider::{MessagesProvider, ProviderRegistry},
    research::research_profile_with_tools,
};
use fs2::FileExt;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
#[path = "support/unmarked.rs"]
mod unmarked;

const MODEL: &str = "opencode-messages/claude-haiku-5-5";
const GLOBAL_LIMIT: usize = 30;
const MAX_CAPTURE_BYTES: usize = 2 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(180);
type CaptureTask = tokio::task::JoinHandle<Result<()>>;
const TWO_ROUNDS_TASK: &str = "Find and verify the fixture's answer. Before your first tool call, use a brief thinking block to verify this constraint arithmetic: six calls include each outer JS call and every nested host call, even cache hits; two JS rounds with two host calls each leave exactly zero ordinary calls; final submission remains available. Follow that verified plan. First JS round: call tools.web_search({query:'recorded fixture answer'}) twice sequentially with identical arguments, save the first result id in state.source and return it. Second JS round: call tools.web_fetch({id:state.source}) twice sequentially with identical arguments and return the evidence. Do not combine the rounds or call other tools. Then call tools.submit_result with the findings/gaps/usable result (submission is allowed after the ordinary-call budget is exhausted): one finding naming the fetched one-word answer, with the returned stable source id in source_ids, empty gaps, usable true.";
const FIRST_ROUND_TASK: &str = "Find and verify the fixture's answer. Before your first tool call, use a brief thinking block to verify this constraint arithmetic: six calls include the outer JS call and every nested host call, even cache hits; one JS with five host calls leaves exactly zero ordinary calls; final submission remains available. Follow that verified plan in one JS round: call tools.web_search({query:'recorded fixture answer'}), save the first result id in state.source, call tools.web_fetch({id:state.source}) once, then call that identical web_fetch three more times sequentially, and return the evidence. Do not call other tools. Then call tools.submit_result with the findings/gaps/usable result (submission is allowed after the ordinary-call budget is exhausted): one finding naming the fetched one-word answer, with the returned stable source id in source_ids, empty gaps, usable true.";

#[derive(Clone, Copy)]
enum ProbeCase {
    TwoRounds,
    FirstRoundBudget,
}
impl ProbeCase {
    fn from_env() -> Result<Self> {
        match std::env::var("BP_RESEARCH_LIVE400_CASE").as_deref() {
            Ok("first_round_budget") => Ok(Self::FirstRoundBudget),
            Ok("two_rounds") | Err(std::env::VarError::NotPresent) => Ok(Self::TwoRounds),
            _ => anyhow::bail!("BP_RESEARCH_LIVE400_CASE must be two_rounds or first_round_budget"),
        }
    }
    fn task(self) -> &'static str {
        match self {
            Self::TwoRounds => TWO_ROUNDS_TASK,
            Self::FirstRoundBudget => FIRST_ROUND_TASK,
        }
    }
    fn minimum_tool_rounds(self) -> usize {
        match self {
            Self::TwoRounds => 2,
            Self::FirstRoundBudget => 1,
        }
    }
}

fn schema() -> Value {
    json!({"name":"research_answer","schema":{"type":"object","properties":{"findings":{"type":"array","items":{"type":"object","properties":{"text":{"type":"string"},"source_ids":{"type":"array","items":{"type":"string"}}},"required":["text","source_ids"],"additionalProperties":false}},"gaps":{"type":"array","items":{"type":"string"}},"usable":{"type":"boolean"}},"required":["findings","gaps","usable"],"additionalProperties":false}})
}

/// Count durably before sending, including unsuccessful attempts. The lock is
/// shared between processes, and an invalid existing counter fails closed.
fn reserve_request(path: &Path) -> Result<usize> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.lock_exclusive()?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let count = if text.trim().is_empty() {
        0
    } else {
        // Append rather than truncate: an interrupted counter write cannot
        // erase prior reservations and silently reset the paid-call ceiling.
        ensure!(text.ends_with('\n'), "incomplete live request count");
        let mut previous = None;
        for line in text.lines() {
            let current = line
                .parse::<usize>()
                .context("invalid live request count")?;
            ensure!(
                previous.is_none_or(|previous| current == previous + 1),
                "invalid live request count sequence"
            );
            previous = Some(current);
        }
        previous.context("invalid live request count")?
    };
    ensure!(
        count < GLOBAL_LIMIT,
        "global live request ceiling reached (30)"
    );
    let next = count + 1;
    writeln!(file, "{next}")?;
    file.sync_all()?;
    FileExt::unlock(&file)?;
    Ok(next)
}

#[test]
fn request_31_is_blocked_across_counter_reopens() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("request-count");
    std::fs::write(&path, "29\n")?;
    assert_eq!(reserve_request(&path)?, 30);
    assert!(reserve_request(&path).is_err());
    assert_eq!(std::fs::read_to_string(path)?, "29\n30\n");
    Ok(())
}

#[derive(Clone)]
struct RecordedStep {
    status: StatusCode,
    response: Vec<u8>,
    request: Option<Value>,
}

/// Decode complete ordered content from the original SSE, independently of the
/// Messages adapter and its provider_state/replay helpers. Comparing these
/// against the next wire request catches a faulty original live replay too.
fn original_content(body: &[u8]) -> Result<Vec<Value>> {
    let text = std::str::from_utf8(body)?;
    let mut blocks = std::collections::BTreeMap::<usize, Value>::new();
    let mut inputs = std::collections::BTreeMap::<usize, String>::new();
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        if data.trim() == "[DONE]" {
            continue;
        }
        let event: Value = serde_json::from_str(data.trim_start())?;
        let index = event["index"].as_u64().unwrap_or_default() as usize;
        match event["type"].as_str() {
            Some("content_block_start") => {
                blocks.insert(index, event["content_block"].clone());
            }
            Some("content_block_delta") => {
                let Some(block) = blocks.get_mut(&index) else {
                    continue;
                };
                let delta = &event["delta"];
                let field = match delta["type"].as_str() {
                    Some("text_delta") => "text",
                    Some("thinking_delta") => "thinking",
                    Some("signature_delta") => "signature",
                    Some("input_json_delta") => {
                        inputs.entry(index).or_default().push_str(
                            delta["partial_json"]
                                .as_str()
                                .context("invalid original tool-input delta")?,
                        );
                        continue;
                    }
                    _ => continue,
                };
                let part = delta[field]
                    .as_str()
                    .context("invalid original thinking/signature delta")?;
                let prior = block[field].as_str().unwrap_or_default();
                block[field] = json!(format!("{prior}{part}"));
            }
            Some("content_block_stop") => {
                if let Some(input) = inputs.remove(&index) {
                    blocks
                        .get_mut(&index)
                        .context("original tool-input block missing")?["input"] =
                        serde_json::from_str(&input)?;
                }
            }
            _ => {}
        }
    }
    Ok(blocks.into_values().collect())
}

fn request_content(request: &Value) -> Vec<Value> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "assistant")
        .flat_map(|message| message["content"].as_array().unwrap())
        .cloned()
        .collect()
}

fn validate_signed_prefix(first: &Value, previous: &Value, current: &Value) -> Result<()> {
    ensure!(
        current["system"] == first["system"],
        "signed conversation prefix changed: system prompt"
    );
    ensure!(
        current["tools"] == first["tools"],
        "signed conversation prefix changed: tool declarations"
    );
    ensure!(
        unmarked::extends(&previous["messages"], &current["messages"]),
        "signed conversation prefix changed: prior message prefix"
    );
    Ok(())
}

fn assert_original_signed_tool_round(step: &RecordedStep) -> Result<()> {
    let blocks = original_content(&step.response)?;
    let thinking = blocks
        .iter()
        .position(|block| {
            block["type"] == "thinking"
                && block["signature"]
                    .as_str()
                    .is_some_and(|signature| !signature.is_empty())
        })
        .context("real first response lacks signed thinking")?;
    let tool = blocks
        .iter()
        .position(|block| block["type"] == "tool_use")
        .context("real first response lacks original tool_use")?;
    ensure!(
        thinking < tool,
        "original signed thinking must precede its original tool_use"
    );
    Ok(())
}

/// These values live only in memory; no header map is persisted or printed.
fn capture_secrets(headers: &HeaderMap) -> Vec<Vec<u8>> {
    let mut secrets = Vec::new();
    for value in headers.get_all("x-api-key").iter() {
        if !value.as_bytes().is_empty() {
            secrets.push(value.as_bytes().to_vec());
        }
    }
    for value in headers.get_all("authorization").iter() {
        if let Ok(value) = value.to_str()
            && let Some((scheme, credential)) = value.split_once(' ')
            && scheme.eq_ignore_ascii_case("bearer")
            && !credential.trim().is_empty()
        {
            secrets.push(credential.trim().as_bytes().to_vec());
        }
    }
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    secrets.dedup();
    secrets
}

fn sanitize_capture(mut body: Vec<u8>, secrets: &[Vec<u8>]) -> Result<Vec<u8>> {
    for secret in secrets {
        if secret.is_empty() {
            continue;
        }
        let mut sanitized = Vec::with_capacity(body.len());
        let mut offset = 0;
        while offset < body.len() {
            if body[offset..].starts_with(secret) {
                sanitized.extend_from_slice(b"[REDACTED]");
                offset += secret.len();
            } else {
                sanitized.push(body[offset]);
                offset += 1;
            }
            ensure!(
                sanitized.len() <= MAX_CAPTURE_BYTES,
                "sanitized response capture exceeds limit"
            );
        }
        body = sanitized;
    }
    // Fail closed for unusual overlapping values or a credential equal to the
    // replacement marker, rather than ever writing a residual credential.
    ensure!(
        secrets.iter().all(|secret| secret.is_empty()
            || !body
                .windows(secret.len())
                .any(|window| window == secret.as_slice())),
        "response capture contains a credential after redaction"
    );
    Ok(body)
}

#[test]
fn capture_redaction_preserves_clean_bytes_and_removes_exact_echoes() -> Result<()> {
    let secrets = vec![
        b"synthetic-private-key".to_vec(),
        b"synthetic-bearer-token".to_vec(),
    ];
    let clean = b"event: message_stop\r\ndata: {}\r\n\r\n".to_vec();
    assert_eq!(sanitize_capture(clean.clone(), &secrets)?, clean);
    let echoed = b"{\"error\":\"synthetic-private-key Bearer synthetic-bearer-token\"}".to_vec();
    assert_eq!(
        sanitize_capture(echoed, &secrets)?,
        b"{\"error\":\"[REDACTED] Bearer [REDACTED]\"}"
    );
    Ok(())
}

#[derive(Clone)]
enum Transport {
    Replay(Arc<Vec<RecordedStep>>),
    Live {
        upstream: String,
        client: reqwest::Client,
        directory: PathBuf,
        counter: PathBuf,
    },
}

#[derive(Clone)]
struct RelayState {
    transport: Transport,
    requests: Arc<Mutex<Vec<Value>>>,
    failures: Arc<Mutex<Vec<String>>>,
    captures: Arc<Mutex<Vec<CaptureTask>>>,
}

async fn relay(State(state): State<RelayState>, headers: HeaderMap, body: Bytes) -> Response {
    match relay_result(&state, headers, body).await {
        Ok(response) => response,
        Err(error) => {
            // Only our fixed context and IO errors can reach this message;
            // reqwest errors are replaced below to avoid credential-bearing URLs.
            let message = error.to_string();
            state.failures.lock().unwrap().push(message.clone());
            if matches!(state.transport, Transport::Replay(_)) {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"type":"error","error":{"type":"invalid_request_error","message":message}})),
                ).into_response()
            } else {
                (StatusCode::BAD_GATEWAY, "research relay failed").into_response()
            }
        }
    }
}

async fn relay_result(state: &RelayState, headers: HeaderMap, body: Bytes) -> Result<Response> {
    let payload: Value = serde_json::from_slice(&body)?;
    ensure!(
        payload["max_tokens"] == 16_384,
        "unexpected output-token limit"
    );
    ensure!(
        payload["model"] == "claude-haiku-5-5",
        "unexpected live model"
    );
    ensure!(
        payload["output_config"]["effort"] == "low",
        "unexpected reasoning effort"
    );
    ensure!(
        payload["output_config"].get("format").is_none(),
        "response schema leaked onto provider wire"
    );
    let index = {
        let mut requests = state.requests.lock().unwrap();
        requests.push(payload.clone());
        requests.len() - 1
    };
    match &state.transport {
        Transport::Replay(steps) => {
            let step = steps
                .get(index)
                .context("recorded research responses exhausted")?;
            let original = steps[..index]
                .iter()
                .filter(|step| step.status.is_success())
                .map(|step| original_content(&step.response))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
            ensure!(
                serde_json::to_vec(&request_content(&payload))? == serde_json::to_vec(&original)?,
                "replayed ordered content (including thinking/signature/tool_use) differs from original response SSE"
            );
            // Check binding only after opaque content matches: the original
            // failure preserved its signature and block order, but changed
            // the signed system prefix on the follow-up request.
            if index > 0 {
                let requests = state.requests.lock().unwrap();
                validate_signed_prefix(&requests[0], &requests[index - 1], &payload)?;
            }
            if let Some(recorded) = &step.request {
                // Runtime UUIDs, local web ports and host notices are disposable.
                // Native assistant replay blocks must remain exactly as recorded.
                let assistants = |value: &Value| -> Vec<Value> {
                    value["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|message| message["role"] == "assistant")
                        .map(|message| message["content"].clone())
                        .collect()
                };
                ensure!(
                    assistants(&payload) == assistants(recorded),
                    "recorded assistant native blocks changed during replay"
                );
            }
            Ok(Response::builder()
                .status(step.status)
                .header(
                    "content-type",
                    if step.status.is_success() {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                )
                .body(Body::from(step.response.clone()))?)
        }
        Transport::Live {
            upstream,
            client,
            directory,
            counter,
        } => {
            let global = reserve_request(counter)?;
            let number = index + 1;
            std::fs::write(directory.join(format!("request-{number:03}.json")), &body)?;
            std::fs::write(
                directory.join(format!("request-{number:03}.global-count")),
                format!("{global}\n"),
            )?;
            let capture_path = directory.join(format!("response-{number:03}.body"));
            let secrets = capture_secrets(&headers);
            let mut request = client.post(format!("{}/messages", upstream.trim_end_matches('/')));
            // Forward incoming headers but never retain, assert or print them.
            for (name, value) in &headers {
                if !matches!(
                    name.as_str(),
                    "host" | "content-length" | "connection" | "transfer-encoding"
                ) {
                    request = request.header(name, value);
                }
            }
            let response = match request.body(body).send().await {
                Ok(response) => response,
                Err(_) => {
                    std::fs::write(
                        directory.join(format!("response-{number:03}.error")),
                        "upstream transport failed\n",
                    )?;
                    anyhow::bail!("upstream transport failed");
                }
            };
            std::fs::write(
                directory.join(format!("response-{number:03}.status")),
                format!("{}\n", response.status().as_u16()),
            )?;
            let mut outgoing = Response::builder().status(response.status());
            for (name, value) in response.headers() {
                if !matches!(
                    name.as_str(),
                    "content-length" | "connection" | "transfer-encoding"
                ) {
                    outgoing = outgoing.header(name, value);
                }
            }
            let failures = state.failures.clone();
            let complete = directory.join(format!("response-{number:03}.complete"));
            let failed = directory.join(format!("response-{number:03}.error"));
            let (sender, receiver) = tokio::sync::mpsc::channel(16);
            let task = tokio::spawn(async move {
                let mut stream = response.bytes_stream();
                let mut captured = Vec::new();
                while let Some(chunk) = stream.next().await {
                    let chunk = match chunk {
                        Ok(chunk) => chunk,
                        Err(_) => {
                            let message = "upstream response stream failed";
                            failures.lock().unwrap().push(message.into());
                            std::fs::write(failed, format!("{message}\n"))?;
                            let _ = sender.send(Err(std::io::Error::other(message))).await;
                            anyhow::bail!(message);
                        }
                    };
                    if captured.len().saturating_add(chunk.len()) > MAX_CAPTURE_BYTES {
                        let message = "response capture exceeds 2 MiB limit";
                        failures.lock().unwrap().push(message.into());
                        std::fs::write(failed, format!("{message}\n"))?;
                        let _ = sender.send(Err(std::io::Error::other(message))).await;
                        anyhow::bail!(message);
                    }
                    captured.extend_from_slice(&chunk);
                    // Keep draining/capturing after the adapter closes its SSE
                    // response. Observation never modifies forwarded bytes.
                    let _ = sender.send(Ok::<_, std::io::Error>(chunk)).await;
                }
                // Nothing from the upstream body reaches disk before all
                // exact credential echoes have been removed, across chunks.
                let sanitized = sanitize_capture(captured, &secrets)?;
                let mut capture = File::create(capture_path)?;
                capture
                    .write_all(&sanitized)
                    .context("response capture failed")?;
                capture.sync_all()?;
                std::fs::write(complete, "complete\n")?;
                Ok(())
            });
            state.captures.lock().unwrap().push(task);
            let stream = futures_util::stream::unfold(receiver, |mut receiver| async {
                receiver.recv().await.map(|chunk| (chunk, receiver))
            });
            Ok(outgoing.body(Body::from_stream(stream))?)
        }
    }
}

struct Server {
    base: String,
    task: tokio::task::JoinHandle<()>,
}
impl Server {
    async fn start(app: Router) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Ok(Self { base, task })
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct LowEffortProvider {
    inner: Arc<dyn Provider>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}
#[async_trait]
impl Provider for LowEffortProvider {
    fn capabilities(&self, model: &str) -> Capabilities {
        self.inner.capabilities(model)
    }
    async fn generate(
        &self,
        mut request: ModelRequest,
        on_text: TextCallback,
        cancel: CancellationToken,
    ) -> Result<ModelResponse> {
        ensure!(
            request.model == MODEL,
            "research child changed provider/model"
        );
        ensure!(
            request.output_schema.is_none()
                && request
                    .tools
                    .iter()
                    .any(|tool| tool.name == "js" && tool.description.contains("submit_result")),
            "child result submission capability was lost"
        );
        request.reasoning_effort = Some(ReasoningEffort::Low);
        request.max_output_tokens = Some(16_384);
        self.requests.lock().unwrap().push(request.clone());
        self.inner.generate(request, on_text, cancel).await
    }
}

async fn run_research_child(transport: Transport, key: String, case: ProbeCase) -> Result<()> {
    let state = RelayState {
        transport,
        requests: Arc::default(),
        failures: Arc::default(),
        captures: Arc::default(),
    };
    let wire = Server::start(
        Router::new()
            .route("/messages", post(relay))
            .with_state(state.clone()),
    )
    .await?;
    // A fake web backend uses local HTTP and ordinary source registration/cache.
    let searches = Arc::new(AtomicUsize::new(0));
    let fetches = Arc::new(AtomicUsize::new(0));
    let page_url = Arc::new(Mutex::new(String::new()));
    let url = page_url.clone();
    let search_count = searches.clone();
    let fetch_count = fetches.clone();
    let web_server = Server::start(Router::new()
        .route("/search", get(move || {
            let url = url.lock().unwrap().clone();
            search_count.fetch_add(1, Ordering::SeqCst);
            async move { Json(json!({"results":[{"title":"Recorded fixture","url":url,"content":"Fetch this page to learn the fixture answer."}]})) }
        }))
        .route("/page", get(move || {
            fetch_count.fetch_add(1, Ordering::SeqCst);
            async { Html("<html><head><title>Recorded fixture</title></head><body><h1>Fixture evidence</h1><p>The fixture's one-word answer is violet.</p></body></html>") }
        }))).await?;
    *page_url.lock().unwrap() = format!("{}/page", web_server.base);
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let parent = store.create_session("live400-parent")?;
    let repl = Arc::new(CodeMode::new(
        directory.path().to_owned(),
        Arc::new(ArtifactStore::new(
            directory.path(),
            directory.path().join("artifacts"),
        )?),
    )?);
    let registry = Arc::new(ProviderRegistry::new(
        "opencode-messages",
        Arc::new(MessagesProvider::api_key(key, &wire.base)?),
    ));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(LowEffortProvider {
        inner: registry,
        requests: requests.clone(),
    });
    let config = HarnessConfig {
        execution_mode: bubblepaw_core::ExecutionMode::Js,
        model: MODEL.into(),
        max_rounds: Some(8),
        ..Default::default()
    };
    let web = WebConfig {
        search: vec![SearchBackend::SearxNg {
            url: web_server.base.clone(),
        }],
        allow_private: true,
        ..Default::default()
    };
    let research = research_profile_with_tools(
        store.clone(),
        provider,
        &ToolRegistry::new(),
        &repl,
        &config,
        &web,
    )?;
    let mut fallback = research.clone();
    fallback.name = "read-only".into();
    let manager = Arc::new(
        AgentTools::from_profiles(store.clone(), vec![fallback, research])?.with_repl(repl.clone()),
    );
    let context = ToolContext {
        session_id: parent.id.clone(),
        run_id: "live400-parent-run".into(),
        invocation_id: "live400-child-launch".into(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    };
    let outcome = async {
        let child = manager.start(
            &context,
            json!({"profile":"research","task":case.task(),"output_schema":schema(),"max_tool_calls":6}),
        )?;
        let result = tokio::time::timeout(
            TIMEOUT,
            manager.wait(&parent.id, child["sessionId"].as_str().unwrap(), None),
        )
        .await??;
        // Terminal failures stay captured on disk before reporting the child error.
        let relay_failures = state.failures.lock().unwrap().clone();
        ensure!(
            result["status"] == "completed",
            "research child status {}: {}; relay: {relay_failures:?}",
            result["status"],
            result["error"]
        );
        assert!(result["json"]["findings"][0]["text"].as_str().is_some_and(|text| text.contains("violet")));
        assert_eq!(result["json"]["usable"], true);
        assert_eq!(result["json"]["gaps"], json!([]));
        assert!(
            result["json"]["findings"][0]["source_ids"][0]
                .as_str()
                .is_some_and(|id| id.starts_with('s'))
        );
        let messages = store.messages(child["sessionId"].as_str().unwrap())?;
        let tool_rounds: Vec<_> = messages
            .iter()
            .filter(|message| message.role == Role::Assistant && !message.tool_calls.is_empty())
            .collect();
        assert!(
            tool_rounds.len() >= case.minimum_tool_rounds(),
            "research did not complete its required model tool rounds"
        );
        assert!(
            tool_rounds
                .iter()
                .all(|message| message.provider_state.is_some()),
            "Messages native replay state did not survive SQLite"
        );
        assert!(
            messages
                .iter()
                .filter(|message| message.role == Role::Tool)
                .all(|message| !message.is_error),
            "research tool failed"
        );
        assert_eq!(searches.load(Ordering::SeqCst), 1);
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        assert!(requests.lock().unwrap().len() > case.minimum_tool_rounds());
        {
            let model_requests = requests.lock().unwrap();
            let last = model_requests.last().context("no final model request")?;
            assert!(!last.tools.is_empty(), "budget-final round lost its declarations");
            assert!(last.disabled_tools.is_empty(), "budget-final round moved tools without a call");
        }
        {
            let wire_requests = state.requests.lock().unwrap();
            let final_request = wire_requests.last().context("no final wire request")?;
            assert_eq!(final_request["tools"], wire_requests[0]["tools"], "budget-final wire declarations changed");
            assert_eq!(final_request["tool_choice"]["type"], "auto", "budget-final wire choice changed without a call");
            assert_eq!(final_request["max_tokens"], 16_384);
        }
        let failures = state.failures.lock().unwrap().clone();
        ensure!(failures.is_empty(), "relay/capture failures: {failures:?}");
        if let Transport::Replay(steps) = &state.transport {
            assert_eq!(
                state.requests.lock().unwrap().len(),
                steps.len(),
                "unused recorded responses"
            );
        }
        Ok(())
    }
    .await;
    manager.shutdown().await?;
    repl.shutdown().await;
    let captures = std::mem::take(&mut *state.captures.lock().unwrap());
    for capture in captures {
        tokio::time::timeout(TIMEOUT, capture).await???;
    }
    outcome
}

fn load_capture(directory: &Path) -> Result<Vec<RecordedStep>> {
    let mut steps = Vec::new();
    for index in 1..=GLOBAL_LIMIT {
        let body = directory.join(format!("response-{index:03}.body"));
        if !body.is_file() {
            break;
        }
        let status =
            std::fs::read_to_string(directory.join(format!("response-{index:03}.status")))?
                .trim()
                .parse::<u16>()?;
        steps.push(RecordedStep {
            status: StatusCode::from_u16(status)?,
            response: std::fs::read(body)?,
            request: Some(serde_json::from_slice(&std::fs::read(
                directory.join(format!("request-{index:03}.json")),
            )?)?),
        });
    }
    ensure!(
        !steps.is_empty(),
        "no recorded research fixture in {}",
        directory.display()
    );
    Ok(steps)
}

#[path = "research_live400/synthetic.rs"]
mod synthetic;

#[tokio::test]
async fn replay_recorded_signed_research_then_synthetic_submission_recovery() -> Result<()> {
    let directory = std::env::var_os("BP_RESEARCH_LIVE400_REPLAY")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/research_live400")
        });
    let mut steps = load_capture(&directory)?;
    assert_original_signed_tool_round(&steps[0])?;
    // Preserve every captured SSE byte. This historical fixture ends in JSON
    // text, so a synthetic follow-up answers the runtime's submission reminder.
    // This mixed replay checks signed history and recovery, not live acceptance.
    let final_text = original_content(&steps.last().context("no captured response")?.response)?
        .iter()
        .filter_map(|block| block["text"].as_str())
        .collect::<String>();
    steps.push(synthetic::submission(serde_json::from_str(&final_text)?));
    run_research_child(
        Transport::Replay(Arc::new(steps)),
        "offline-fixture-key".into(),
        ProbeCase::TwoRounds,
    )
    .await
}

#[path = "research_live400/live_capture.rs"]
mod live_capture;
