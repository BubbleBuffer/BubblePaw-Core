//! Explicitly selected live qualification; ordinary test runs make no model calls.
//!
//! Run only after authorizing paid calls:
//! `cargo +1.97.1 test -p bubblepaw-runner --test provider_live -- --ignored --test-threads=1`
//! Select one kind/scenario with a filter such as `opencode_messages::tools`.
//! Every request asks for low reasoning effort. Prompt instructions are under
//! 200 input tokens, excluding tool/schema definitions, replay and SDK overhead.
//!
//! Gates and default models (override with the exact BP_LIVE_<KIND>_MODEL name):
//! - codex: BP_LIVE_CODEX=1 and ~/.codex/auth.json; gpt-6-luna;
//!   BP_LIVE_CODEX_MODEL. The harness never reads the login file itself.
//! - responses: OPENAI_API_KEY; gpt-6-luna; BP_LIVE_RESPONSES_MODEL.
//!   BP_LIVE_RESPONSES_BASE_URL (default: the config `api` provider) and
//!   BP_LIVE_RESPONSES_KEY_ENV (default OPENAI_API_KEY) retarget the same wire,
//!   e.g. OpenCode Go with base https://opencode.ai/zen/go/v1 and key
//!   OPENCODE_API_KEY.
//! - anthropic_messages: ANTHROPIC_API_KEY; claude-haiku-5-5;
//!   BP_LIVE_ANTHROPIC_MESSAGES_MODEL.
//! - opencode_messages: OPENCODE_API_KEY; claude-haiku-5-5;
//!   BP_LIVE_OPENCODE_MESSAGES_MODEL.
//! - opencode_chat: OPENCODE_API_KEY; deepseek-v4.1-flash;
//!   BP_LIVE_OPENCODE_CHAT_MODEL.
//! - openrouter_chat: OPENROUTER_API_KEY and BP_LIVE_OPENROUTER_CHAT_MODEL.
//!   BP_LIVE_OPENROUTER_CHAT_BASE_URL (default https://openrouter.ai/api/v1)
//!   retargets the same chat wire.
//! - deepseek_chat: DEEPSEEK_API_KEY and BP_LIVE_DEEPSEEK_CHAT_MODEL.
//!   These optional endpoints have no model default in config.rs, so selecting
//!   their model explicitly avoids inventing a billable model choice.
//! - claude_code: BP_LIVE_CLAUDE_CODE=1 and bun on PATH; haiku;
//!   BP_LIVE_CLAUDE_CODE_MODEL. Login is left entirely to Claude Code.
//!
//! Per HTTP kind, stream=1, tools=2, cancel=1, schema=1 and bad_key=1 upstream
//! requests on success. A loopback forwarding relay caps each scenario before
//! sending an extra request; it forwards original HTTP headers, JSON and SSE.
//! No credential headers, response text or opaque reasoning are printed.
//! Codex bad_key is skipped; its continuation is n/a because the subscription
//! endpoint rejects max_output_tokens, which this adapter intentionally omits.
//! Other HTTP continuation tests set the role's max_output_tokens=64, retaining
//! production's four-continuation limit (five total requests). No stop event is
//! rewritten. Prior rounds must stop on length; the final round must complete
//! or still stop on length only at the five-request ceiling. A roughly 150-word
//! answer usually needs 2–4 requests; reasoning can consume the entire budget.
//!
//! Upstream request budgets when all gates are enabled (min / expected / max):
//! | Kind | Non-continuation | Continuation | Total |
//! | --- | --- | --- | --- |
//! | codex | 5 | n/a | 5 / 5 / 5 |
//! | responses | 6 | 1 / 2–4 / 5 | 7 / 8–10 / 11 |
//! | anthropic_messages | 6 | 1 / 2–4 / 5 | 7 / 8–10 / 11 |
//! | opencode_messages | 6 | 1 / 2–4 / 5 | 7 / 8–10 / 11 |
//! | opencode_chat | 6 | 1 / 2–4 / 5 | 7 / 8–10 / 11 |
//! | openrouter_chat | 6 | 1 / 2–4 / 5 | 7 / 8–10 / 11 |
//! | deepseek_chat | 6 | 1 / 2–4 / 5 | 7 / 8–10 / 11 |
//! | claude_code | 5 host steps | SDK-owned, n/a | 5 / 5 / 5 host steps; SDK requests unknown |
//!
//! Total HTTP requests: min 47, expected 53–65, max 71, including six expected
//! unbilled invalid-key auth probes. These are budgets/count assertions, not
//! measurements of paid runs. Failed auth is not a promise of provider billing.
//! Claude Code stream=1, tools=2, cancel=1, schema=1 extension `step` calls;
//! continuation and bad_key are skipped. The SDK owns its internal model turns,
//! so five steps are not a claim of five billable requests. No SDK-level request
//! or token cap is implied. Cancellation can be billed without final usage.
//!
//! Rough planning token ranges per HTTP request (input includes definitions
//! and replay; output includes reasoning): stream 20–200 / 5–300; tools round
//! one 80–400 / 20–1,000; tools round two 70–600 / 10–500;
//! cancel 20–200 / 0–1,000; schema
//! 200–500 / 20–500; invalid-key probe zero accepted model tokens expected.
//! Continuation adds roughly 30–800 input per request including prior rounds;
//! its native output limit is 64 per request, at most 320 across five requests.
//! Whether that limit includes reasoning depends on provider/model semantics;
//! output usage is observed rather than asserted to be <=64. Without
//! continuation, roughly 390–1,900 input and 55–3,300 output per HTTP kind is
//! estimated, excluding auth probes. These are estimates, not cost limits;
//! reasoning, cache accounting and first-chunk timing can exceed them. Claude
//! Code's SDK/system/tool prefix can add several thousand input tokens per
//! internal call, so the HTTP estimate does not describe its billable usage.

use anyhow::{Result, ensure};
use async_trait::async_trait;
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use bubblepaw_core::{
    CancellationToken, Capabilities, Harness, HarnessConfig, JsonSchema, ModelRequest,
    ModelResponse, Provider, ProviderError, ProviderErrorKind, ReasoningEffort, ReasoningReplay,
    Role, RunOptions, Store, TextCallback, Tool, ToolContext, ToolDefinition, ToolRegistry,
    ToolResult, provider_state_replay_compatible,
};
use bubblepaw_runner::{
    config::{EffectiveConfig, LoadOptions},
    provider::{
        ChatCompletionsProvider, ExtensionConfig, ExtensionProvider, MessagesProvider,
        ResponsesProvider,
    },
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

const TEST_TIMEOUT: Duration = Duration::from_secs(180);
const MAX_CONTINUATION_REQUESTS: usize = 5;
const MAX_CAPTURE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Codex,
    Responses,
    AnthropicMessages,
    OpencodeMessages,
    OpencodeChat,
    OpenrouterChat,
    DeepseekChat,
    ClaudeCode,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Responses => "responses",
            Self::AnthropicMessages => "anthropic_messages",
            Self::OpencodeMessages => "opencode_messages",
            Self::OpencodeChat => "opencode_chat",
            Self::OpenrouterChat => "openrouter_chat",
            Self::DeepseekChat => "deepseek_chat",
            Self::ClaudeCode => "claude_code",
        }
    }

    fn model_var(self) -> &'static str {
        match self {
            Self::Codex => "BP_LIVE_CODEX_MODEL",
            Self::Responses => "BP_LIVE_RESPONSES_MODEL",
            Self::AnthropicMessages => "BP_LIVE_ANTHROPIC_MESSAGES_MODEL",
            Self::OpencodeMessages => "BP_LIVE_OPENCODE_MESSAGES_MODEL",
            Self::OpencodeChat => "BP_LIVE_OPENCODE_CHAT_MODEL",
            Self::OpenrouterChat => "BP_LIVE_OPENROUTER_CHAT_MODEL",
            Self::DeepseekChat => "BP_LIVE_DEEPSEEK_CHAT_MODEL",
            Self::ClaudeCode => "BP_LIVE_CLAUDE_CODE_MODEL",
        }
    }

    fn key_var(self) -> Option<&'static str> {
        match self {
            Self::Codex | Self::ClaudeCode => None,
            Self::Responses => Some("OPENAI_API_KEY"),
            Self::AnthropicMessages => Some("ANTHROPIC_API_KEY"),
            Self::OpencodeMessages | Self::OpencodeChat => Some("OPENCODE_API_KEY"),
            Self::OpenrouterChat => Some("OPENROUTER_API_KEY"),
            Self::DeepseekChat => Some("DEEPSEEK_API_KEY"),
        }
    }

    /// The env var that holds this kind's key. Responses honours
    /// BP_LIVE_RESPONSES_KEY_ENV; every other kind uses `key_var`.
    fn key_env(self) -> Option<String> {
        if self == Self::Responses {
            return Some(
                std::env::var("BP_LIVE_RESPONSES_KEY_ENV")
                    .ok()
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| "OPENAI_API_KEY".into()),
            );
        }
        self.key_var().map(str::to_owned)
    }

    fn adapter(self) -> &'static str {
        match self {
            Self::Codex | Self::Responses => "responses",
            Self::AnthropicMessages | Self::OpencodeMessages | Self::ClaudeCode => {
                "anthropic-messages"
            }
            Self::OpencodeChat | Self::OpenrouterChat | Self::DeepseekChat => "chat-completions",
        }
    }
}

fn skipped(kind: Kind, reason: &str) {
    println!("skipped: {}: {reason}", kind.name());
}

fn env_present(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .is_some_and(|value| !value.trim().is_empty())
}

fn codex_auth_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|home| PathBuf::from(home).join(".codex/auth.json"))
}

fn bun_on_path() -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|path| {
            path.join(if cfg!(windows) { "bun.exe" } else { "bun" })
                .is_file()
        })
    })
}

/// BP_LIVE_RESPONSES_BASE_URL retargets the Responses wire; empty means default.
fn responses_base_url() -> Option<String> {
    std::env::var("BP_LIVE_RESPONSES_BASE_URL")
        .ok()
        .map(|url| url.trim().trim_end_matches('/').to_owned())
        .filter(|url| !url.is_empty())
}

/// BP_LIVE_OPENROUTER_CHAT_BASE_URL retargets the OpenRouter chat wire; empty means default.
fn openrouter_chat_base_url() -> Option<String> {
    std::env::var("BP_LIVE_OPENROUTER_CHAT_BASE_URL")
        .ok()
        .map(|url| url.trim().trim_end_matches('/').to_owned())
        .filter(|url| !url.is_empty())
}

fn gate(kind: Kind) -> bool {
    if let Some(var) = kind.key_env() {
        if !env_present(&var) {
            skipped(kind, &format!("{var} not set"));
            return false;
        }
    } else {
        let switch = if kind == Kind::Codex {
            "BP_LIVE_CODEX"
        } else {
            "BP_LIVE_CLAUDE_CODE"
        };
        if std::env::var(switch).ok().as_deref() != Some("1") {
            skipped(kind, &format!("{switch}=1 not set"));
            return false;
        }
        if kind == Kind::Codex && !codex_auth_path().is_some_and(|path| path.is_file()) {
            skipped(kind, "~/.codex/auth.json not present");
            return false;
        }
        if kind == Kind::ClaudeCode && !bun_on_path() {
            skipped(kind, "bun not on PATH");
            return false;
        }
    }
    if matches!(kind, Kind::OpenrouterChat | Kind::DeepseekChat) && !env_present(kind.model_var()) {
        skipped(
            kind,
            &format!("{} not set (no config default)", kind.model_var()),
        );
        return false;
    }
    true
}

/// The relay never retains or inspects credential headers. Its only retained
/// data is model-request JSON and continuation response bytes for native
/// assertions; none is printed. Observation never modifies upstream SSE.
#[derive(Clone)]
struct RelayState {
    upstream: String,
    client: reqwest::Client,
    limit: usize,
    requests: Arc<Mutex<Vec<Value>>>,
    replies: Arc<Mutex<Vec<Arc<WireBody>>>>,
    blocked: Arc<AtomicUsize>,
}

#[derive(Default)]
struct WireBody {
    bytes: Mutex<Vec<u8>>,
    overflow: AtomicBool,
}

async fn forward(
    State(state): State<RelayState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
        return (StatusCode::BAD_REQUEST, "live relay expected JSON").into_response();
    };
    let capture = {
        let mut requests = state.requests.lock().unwrap();
        if requests.len() >= state.limit {
            state.blocked.fetch_add(1, Ordering::SeqCst);
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":{"message":"live harness request budget reached"}})),
            )
                .into_response();
        }
        requests.push(payload);
        if state.limit == MAX_CONTINUATION_REQUESTS {
            let capture = Arc::new(WireBody::default());
            state.replies.lock().unwrap().push(capture.clone());
            Some(capture)
        } else {
            None
        }
    };
    let mut request = state
        .client
        .post(format!("{}{}", state.upstream, uri.path()));
    for (name, value) in &headers {
        if !matches!(
            name.as_str(),
            "host" | "content-length" | "connection" | "transfer-encoding"
        ) {
            request = request.header(name, value);
        }
    }
    let Ok(upstream) = request.body(body).send().await else {
        return (
            StatusCode::BAD_GATEWAY,
            "live relay upstream transport failed",
        )
            .into_response();
    };
    let mut response = Response::builder().status(upstream.status());
    for (name, value) in upstream.headers() {
        if !matches!(
            name.as_str(),
            "content-length" | "connection" | "transfer-encoding"
        ) {
            response = response.header(name, value);
        }
    }
    let stream = upstream.bytes_stream().inspect(move |chunk| {
        if let (Some(capture), Ok(chunk)) = (&capture, chunk) {
            let mut bytes = capture.bytes.lock().unwrap();
            if bytes.len().saturating_add(chunk.len()) <= MAX_CAPTURE_BYTES {
                bytes.extend_from_slice(chunk);
            } else {
                capture.overflow.store(true, Ordering::SeqCst);
            }
        }
    });
    response.body(Body::from_stream(stream)).unwrap()
}

struct Relay {
    state: RelayState,
    task: tokio::task::JoinHandle<()>,
    base: String,
}

impl Relay {
    async fn new(upstream: String, limit: usize) -> Self {
        let state = RelayState {
            upstream: upstream.trim_end_matches('/').into(),
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(TEST_TIMEOUT)
                .build()
                .unwrap(),
            limit,
            requests: Arc::default(),
            replies: Arc::default(),
            blocked: Arc::default(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/{*path}", post(forward))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { state, task, base }
    }

    fn assert_count(&self, expected: usize) {
        assert_eq!(
            self.state.requests.lock().unwrap().len(),
            expected,
            "unexpected upstream request count"
        );
        assert_eq!(
            self.state.blocked.load(Ordering::SeqCst),
            0,
            "adapter exceeded live request budget"
        );
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        eprintln!(
            "live upstream requests: {}; blocked before forwarding: {}",
            self.state.requests.lock().unwrap().len(),
            self.state.blocked.load(Ordering::SeqCst)
        );
        self.task.abort();
    }
}

#[derive(Default)]
struct Witness {
    requests: Mutex<Vec<ModelRequest>>,
    responses: Mutex<Vec<ModelResponse>>,
    text: Mutex<String>,
    chunks: AtomicUsize,
    cancellation_sent: AtomicBool,
}

/// Observes the real provider boundary. Harness still owns tool dispatch,
/// SQLite writes, history preparation and the cancellation terminal state.
struct ObservedProvider {
    inner: Arc<dyn Provider>,
    witness: Arc<Witness>,
    steps: usize,
    cancel_first_chunk: bool,
}

#[async_trait]
impl Provider for ObservedProvider {
    fn capabilities(&self, model: &str) -> Capabilities {
        self.inner.capabilities(model)
    }

    async fn generate(
        &self,
        mut request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        request.reasoning_effort = Some(ReasoningEffort::Low);
        {
            let mut requests = self.witness.requests.lock().unwrap();
            ensure!(
                requests.len() < self.steps,
                "live harness step budget reached"
            );
            requests.push(request.clone());
        }
        let witness = self.witness.clone();
        let cancel = cancellation.clone();
        let cancel_first = self.cancel_first_chunk;
        let response = self
            .inner
            .generate(
                request,
                Arc::new(move |text| {
                    if !text.is_empty() {
                        witness.chunks.fetch_add(1, Ordering::SeqCst);
                        witness.text.lock().unwrap().push_str(text);
                    }
                    on_text(text);
                    if cancel_first
                        && !text.is_empty()
                        && !witness.cancellation_sent.swap(true, Ordering::SeqCst)
                    {
                        cancel.cancel();
                    }
                }),
                cancellation.clone(),
            )
            .await?;
        // Some adapters flush their final listener batch in the same poll as
        // returning success. Cancellation from that batch must still win here.
        ensure!(
            !cancellation.is_cancelled(),
            "Cancelled after listener callback"
        );
        self.witness
            .responses
            .lock()
            .unwrap()
            .push(response.clone());
        Ok(response)
    }

    async fn close_session(&self, session_id: &str) -> Result<()> {
        self.inner.close_session(session_id).await
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    store: Store,
    model: String,
    provider: Arc<dyn Provider>,
    witness: Arc<Witness>,
    relay: Option<Relay>,
}

impl Fixture {
    async fn new(
        kind: Kind,
        steps: usize,
        upstream_limit: usize,
        cancel_first_chunk: bool,
        bad_key: bool,
    ) -> Option<Self> {
        if !gate(kind) {
            return None;
        }
        let credential = if let Some(var) = kind.key_env() {
            match std::env::var(&var)
                .ok()
                .filter(|key| !key.trim().is_empty())
            {
                Some(key) => Some(key),
                None => {
                    skipped(kind, &format!("{var} not set"));
                    return None;
                }
            }
        } else {
            None
        };
        let root = tempfile::tempdir().unwrap();
        // Read defaults only, without user/project config or ambient env overrides.
        let settings = EffectiveConfig::load(LoadOptions {
            workspace: root.path().to_owned(),
            home: None,
            custom_file: None,
            env: Vec::new(),
            cli: Vec::new(),
        })
        .unwrap()
        .settings()
        .unwrap();
        let model = std::env::var(kind.model_var())
            .ok()
            .filter(|model| !model.trim().is_empty())
            .unwrap_or_else(|| match kind {
                Kind::Codex | Kind::Responses => settings.main_model.clone(),
                Kind::AnthropicMessages | Kind::OpencodeMessages => "claude-haiku-5-5".into(),
                Kind::OpencodeChat => settings.roles.summary.models[0]
                    .strip_prefix("opencode/")
                    .unwrap()
                    .into(),
                Kind::ClaudeCode => "haiku".into(),
                Kind::OpenrouterChat | Kind::DeepseekChat => unreachable!("model gate"),
            });
        let upstream = match kind {
            Kind::Codex => settings.providers["codex"].base_url.clone().unwrap(),
            Kind::Responses => responses_base_url()
                .unwrap_or_else(|| settings.providers["api"].base_url.clone().unwrap()),
            Kind::OpencodeChat => settings.providers["opencode"].base_url.clone().unwrap(),
            Kind::OpencodeMessages => settings.providers["opencode-messages"]
                .base_url
                .clone()
                .unwrap(),
            Kind::AnthropicMessages => "https://api.anthropic.com/v1".into(),
            Kind::OpenrouterChat => {
                openrouter_chat_base_url().unwrap_or_else(|| "https://openrouter.ai/api/v1".into())
            }
            Kind::DeepseekChat => "https://api.deepseek.com/v1".into(),
            Kind::ClaudeCode => String::new(),
        };
        let relay = if kind == Kind::ClaudeCode {
            None
        } else {
            Some(Relay::new(upstream, upstream_limit).await)
        };
        let base = relay
            .as_ref()
            .map(|relay| relay.base.clone())
            .unwrap_or_default();
        let key = || {
            if bad_key {
                "bp-live-deliberately-invalid-key".to_owned()
            } else {
                credential
                    .as_ref()
                    .expect("API kind credential gate")
                    .clone()
            }
        };
        let inner: Result<Arc<dyn Provider>> = match kind {
            Kind::Codex => ResponsesProvider::codex_auth(codex_auth_path().unwrap(), base)
                .map(|p| Arc::new(p) as Arc<dyn Provider>),
            Kind::Responses => ResponsesProvider::api_key(key(), base).map(|p| {
                // An explicit Responses base override is the OpenCode Go route.
                let p = if responses_base_url().is_some() {
                    p.with_opencode_route()
                } else {
                    p
                };
                Arc::new(p) as Arc<dyn Provider>
            }),
            Kind::AnthropicMessages => {
                MessagesProvider::api_key(key(), base).map(|p| Arc::new(p) as Arc<dyn Provider>)
            }
            Kind::OpencodeMessages => MessagesProvider::api_key(key(), base)
                .map(|p| Arc::new(p.with_opencode_route()) as Arc<dyn Provider>),
            Kind::OpencodeChat | Kind::OpenrouterChat | Kind::DeepseekChat => {
                ChatCompletionsProvider::api_key(key(), base)
                    .and_then(|p| {
                        let p = if kind == Kind::OpencodeChat {
                            p.with_opencode_route().with_reasoning_fields(
                                settings.endpoints.opencode_reasoning_fields.clone(),
                            )?
                        } else {
                            p
                        };
                        p.with_reasoning_replay(BTreeMap::from([(
                            model.clone(),
                            ReasoningReplay::All,
                        )]))
                    })
                    .map(|p| Arc::new(p) as Arc<dyn Provider>)
            }
            Kind::ClaudeCode => Ok(Arc::new(ExtensionProvider::new(ExtensionConfig {
                id: "claude-code".into(),
                command: "bun".into(),
                args: vec!["run".into(), "src/main.ts".into()],
                cwd: Some(
                    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extensions/claude-code"),
                ),
                ..Default::default()
            }))),
        };
        let inner = inner.unwrap_or_else(|_| {
            panic!("live provider initialization failed; credential details suppressed")
        });
        let witness = Arc::new(Witness::default());
        let provider = Arc::new(ObservedProvider {
            inner,
            witness: witness.clone(),
            steps,
            cancel_first_chunk,
        });
        let db = root.path().join("live.sqlite");
        let store = Store::open(&db).unwrap();
        Some(Self {
            _root: root,
            store,
            model,
            provider,
            witness,
            relay,
        })
    }

    fn harness(&self, tools: ToolRegistry, system: &str, rounds: usize) -> Harness {
        Harness::new(
            self.store.clone(),
            self.provider.clone(),
            tools,
            HarnessConfig {
                model: self.model.clone(),
                system_prompt: system.into(),
                max_rounds: Some(rounds),
                ..Default::default()
            },
        )
    }

    fn assert_counts(&self, steps: usize, upstream: usize) {
        assert_eq!(self.witness.requests.lock().unwrap().len(), steps);
        if let Some(relay) = &self.relay {
            relay.assert_count(upstream);
        }
    }

    fn stored_messages(&self, session: &str) -> Vec<bubblepaw_core::Message> {
        // Store decodes its canonical SQLite records (including blob-backed
        // reasoning). A separate read-only connection proves the durable row
        // count; Store::open alone would reuse the in-process owner connection.
        let path = self
            .store
            .chat_path(session)
            .unwrap()
            .expect("chat archive path missing");
        let disk =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        let count: i64 = disk
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id=?1",
                [session],
                |row| row.get(0),
            )
            .unwrap();
        let messages = self.store.messages(session).unwrap();
        assert_eq!(
            messages.len(),
            usize::try_from(count).expect("invalid SQLite row count"),
            "SQLite row count differs from decoded archive"
        );
        messages
    }
}

async fn prompt(
    harness: &Harness,
    session: &str,
    text: &str,
    options: RunOptions,
) -> bubblepaw_core::Run {
    tokio::time::timeout(TEST_TIMEOUT, harness.prompt(session, text, options))
        .await
        .expect("live scenario exceeded 180 seconds")
        .unwrap_or_else(|_| panic!("live harness failed; provider details suppressed"))
}

async fn stream(kind: Kind) {
    let Some(fixture) = Fixture::new(kind, 1, 1, false, false).await else {
        return;
    };
    let session = fixture.store.create_session("live stream").unwrap();
    let harness = fixture.harness(
        ToolRegistry::new(),
        "Answer the user's question briefly.",
        1,
    );
    let run = prompt(
        &harness,
        &session.id,
        "What is two plus three? Reply in one short sentence.",
        RunOptions::default(),
    )
    .await;
    assert!(
        run.status == "completed",
        "live stream did not complete: status={} error={}",
        run.status,
        run.error.as_deref().unwrap_or("none")
    );
    assert!(
        fixture.witness.chunks.load(Ordering::SeqCst) > 0,
        "no listener text chunks"
    );
    let responses = fixture.witness.responses.lock().unwrap().clone();
    assert!(
        responses.len() == 1 && !responses[0].text.trim().is_empty(),
        "no complete final response"
    );
    assert!(
        *fixture.witness.text.lock().unwrap() == responses[0].text,
        "listener differs from complete response"
    );
    let usage = run.usage.as_ref().expect("missing final usage");
    assert!(
        usage.input_tokens > 0 && usage.output_tokens > 0,
        "missing input/output usage"
    );
    let stored = fixture.stored_messages(&session.id);
    assert!(
        stored.len() == 2
            && stored[1].role == Role::Assistant
            && stored[1].content == responses[0].text,
        "complete final response not persisted"
    );
    fixture.assert_counts(1, 1);
    fixture.provider.close_session(&session.id).await.unwrap();
}

struct Add(Arc<AtomicUsize>);

#[async_trait]
impl Tool for Add {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "add".into(),
            description: "Return the integer sum of a and b.".into(),
            input_schema: json!({"type":"object","additionalProperties":false,"properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"]}),
            read_only: true,
            approval: Default::default(),
        }
    }

    async fn invoke(&self, _context: ToolContext, input: Value) -> Result<ToolResult> {
        ensure!(
            input["a"] == 17 && input["b"] == 25,
            "unexpected live add arguments"
        );
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::text("42"))
    }
}

/// Extract opaque artifacts, without reconstructing ordinary text/tool blocks.
fn artifacts(state: &Value) -> Vec<Value> {
    let mut result = Vec::new();
    for field in ["items", "content"] {
        if let Some(blocks) = state[field].as_array() {
            result.extend(
                blocks
                    .iter()
                    .filter(|block| {
                        matches!(
                            block["type"].as_str(),
                            Some("reasoning" | "thinking" | "redacted_thinking")
                        )
                    })
                    .cloned(),
            );
        }
    }
    for field in ["reasoning_content", "reasoning"] {
        if let Some(reasoning) = state
            .get(field)
            .filter(|value| !value.is_null() && value.as_str().is_none_or(|text| !text.is_empty()))
        {
            result.push(reasoning.clone());
        }
    }
    if let Some(fields) = state.get("message_fields") {
        result.extend(artifacts(fields));
        if let Some(details) = fields["reasoning_details"].as_array() {
            result.extend(details.iter().cloned());
        }
    }
    if let Some(rounds) = state["rounds"].as_array() {
        for round in rounds {
            result.extend(artifacts(round));
        }
    }
    result
}

fn contains_exact(value: &Value, artifact: &Value) -> bool {
    value == artifact
        || match value {
            Value::Array(values) => values.iter().any(|value| contains_exact(value, artifact)),
            Value::Object(fields) => fields.values().any(|value| contains_exact(value, artifact)),
            _ => false,
        }
}

async fn tools(kind: Kind) {
    let Some(fixture) = Fixture::new(kind, 2, 2, false, false).await else {
        return;
    };
    let executions = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry
        .register(Arc::new(Add(executions.clone())))
        .unwrap();
    let tool_name = "add";
    let session = fixture
        .store
        .create_session("live add and reasoning replay")
        .unwrap();
    let system = "Call add exactly once with a=17 and b=25. After its result, reply with the sum and make no further calls.";
    let harness = fixture.harness(registry, system, 2);
    let run = prompt(
        &harness,
        &session.id,
        "Use the add tool to calculate the sum now.",
        RunOptions::default(),
    )
    .await;
    assert!(
        run.status == "completed",
        "live tool round trip did not complete"
    );
    assert!(
        run.text.as_deref().is_some_and(|text| text.contains("42")),
        "final response lacks sum"
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "add was not executed once"
    );
    fixture.assert_counts(2, 2);
    let messages = fixture.stored_messages(&session.id);
    assert!(
        messages.len() == 4,
        "expected input, tool-call assistant, tool result and final assistant"
    );
    let first = &messages[1];
    assert!(
        first.role == Role::Assistant
            && first.tool_calls.len() == 1
            && first.tool_calls[0].name == tool_name,
        "native tool call not stored"
    );
    assert!(
        messages[2].role == Role::Tool
            && !messages[2].is_error
            && messages[2].content.contains("42")
            && messages[2].tool_call_id.as_deref() == Some(first.tool_calls[0].id.as_str()),
        "tool result not durably tied to its call"
    );
    let responses = fixture.witness.responses.lock().unwrap().clone();
    let state = first
        .provider_state
        .as_ref()
        .expect("round one native state missing");
    assert!(
        serde_json::to_vec(state).unwrap()
            == serde_json::to_vec(
                responses[0]
                    .provider_state
                    .as_ref()
                    .expect("native response state missing")
            )
            .unwrap(),
        "round one native state changed in SQLite"
    );
    assert!(
        provider_state_replay_compatible(state, kind.adapter(), &fixture.model),
        "reasoning state origin differs from provider/model"
    );
    let opaque = artifacts(state);
    if opaque.is_empty() {
        skipped(
            kind,
            "reasoning replay check only: model returned no replayable artifact at low effort; tool round trip and native-state persistence checks still apply",
        );
    } else {
        eprintln!("reasoning replay checked: {} artifacts", opaque.len());
    }
    let requests = fixture.witness.requests.lock().unwrap().clone();
    assert!(
        requests[0].model == requests[1].model && requests[1].session_id == session.id,
        "round two switched model/session"
    );
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.id == first.id && message == first),
        "SQLite assistant/state did not reach round two"
    );
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message == &messages[2]),
        "stored tool result did not reach round two"
    );
    if let Some(relay) = &fixture.relay {
        let wire = relay.state.requests.lock().unwrap().clone();
        assert!(
            wire[0]["model"] == wire[1]["model"],
            "native model changed between requests"
        );
        if !opaque.is_empty() {
            assert!(
                opaque
                    .iter()
                    .all(|artifact| contains_exact(&wire[1], artifact)),
                "stored opaque reasoning was not replayed byte-exact on native wire"
            );
        }
    }
    // Claude Code consumes this real stored state at its stateful mirror boundary;
    // it owns the SDK transport, which the HTTP relay cannot inspect.
    fixture.provider.close_session(&session.id).await.unwrap();
}

async fn cancel(kind: Kind) {
    let Some(fixture) = Fixture::new(kind, 1, 1, true, false).await else {
        return;
    };
    let session = fixture.store.create_session("live cancellation").unwrap();
    let harness = fixture.harness(
        ToolRegistry::new(),
        "Write the requested long answer, beginning immediately with the first item.",
        1,
    );
    let run = prompt(
        &harness,
        &session.id,
        "Explain 100 distinct gardening tips in full paragraphs.",
        RunOptions::default(),
    )
    .await;
    assert!(
        fixture.witness.chunks.load(Ordering::SeqCst) > 0
            && fixture.witness.cancellation_sent.load(Ordering::SeqCst),
        "first listener chunk did not trigger cancellation"
    );
    assert!(run.status == "cancelled", "run did not settle cancelled");
    let messages = fixture.stored_messages(&session.id);
    assert!(
        messages.len() == 1 && messages[0].role == Role::User,
        "cancelled stream stored a partial assistant message"
    );
    let stored_run = fixture.store.run(&run.id).unwrap();
    assert!(
        stored_run.status == "cancelled" && stored_run.text.is_none(),
        "cancelled run persisted partial final text"
    );
    fixture.assert_counts(1, 1);
    fixture.provider.close_session(&session.id).await.unwrap();
}

#[path = "provider_live/submission.rs"]
mod submission;
use submission::schema;

async fn bad_key(kind: Kind) {
    if !gate(kind) {
        return;
    }
    if matches!(kind, Kind::Codex | Kind::ClaudeCode) {
        skipped(kind, "bad_key not applicable to file/subscription login");
        return;
    }
    let Some(fixture) = Fixture::new(kind, 1, 1, false, true).await else {
        return;
    };
    let request = ModelRequest {
        model: fixture.model.clone(),
        system_prompt: "Reply briefly.".into(),
        messages: vec![bubblepaw_core::Message {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: "live-invalid-key".into(),
            role: Role::User,
            origin: None,
            content: "Say ready.".into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: String::new(),
        }],
        session_id: "live-invalid-key".into(),
        reasoning_effort: Some(ReasoningEffort::Low),
        ..Default::default()
    };
    let outcome = tokio::time::timeout(
        TEST_TIMEOUT,
        fixture
            .provider
            .generate(request, Arc::new(|_| {}), CancellationToken::new()),
    )
    .await
    .expect("bad-key request timed out");
    let error = match outcome {
        Err(error) => error,
        Ok(_) => panic!("deliberately invalid credential unexpectedly succeeded"),
    };
    assert!(
        ProviderError::of(&error).is_some_and(|error| error.kind == ProviderErrorKind::Auth),
        "invalid credential did not map to Auth"
    );
    assert_eq!(fixture.witness.chunks.load(Ordering::SeqCst), 0);
    fixture.assert_counts(1, 1);
}

async fn continuation(kind: Kind) {
    if !gate(kind) {
        return;
    }
    match kind {
        Kind::Codex => {
            skipped(
                kind,
                "continuation n/a: subscription endpoint rejects max_output_tokens",
            );
            return;
        }
        Kind::ClaudeCode => {
            skipped(kind, "continuation n/a: Claude Code SDK owns its turn");
            return;
        }
        _ => {}
    }
    let Some(fixture) = Fixture::new(kind, 1, MAX_CONTINUATION_REQUESTS, false, false).await else {
        return;
    };
    let session = fixture
        .store
        .create_session("live bounded continuation")
        .unwrap();
    let harness = fixture.harness(
        ToolRegistry::new(),
        "Answer directly, with no introduction or conclusion. If interrupted by an output limit, continue exactly where you stopped.",
        1,
    );
    let run = prompt(
        &harness,
        &session.id,
        "Explain how to plant and care for a small herb garden in about 150 words of ordinary prose.",
        RunOptions { max_output_tokens: Some(64), ..Default::default() },
    ).await;
    assert!(
        run.status == "completed",
        "continuation did not return a joined reply"
    );
    let relay = fixture
        .relay
        .as_ref()
        .expect("HTTP continuation relay missing");
    let requests = relay.state.requests.lock().unwrap().clone();
    let count = requests.len();
    assert!(
        (1..=MAX_CONTINUATION_REQUESTS).contains(&count),
        "continuation request count outside native limit"
    );
    fixture.assert_counts(1, count);
    let limit_field = if kind == Kind::Responses {
        "max_output_tokens"
    } else {
        "max_tokens"
    };
    for request in &requests {
        assert!(
            request[limit_field] == 64 && request["model"] == fixture.model,
            "continuation lost role output limit or model"
        );
    }
    for request in &requests[1..] {
        assert!(
            contains_exact(request, &json!("Continue exactly where you stopped.")),
            "length-stop request lacks native continuation prompt"
        );
    }
    let captures = relay.state.replies.lock().unwrap().clone();
    assert_eq!(
        captures.len(),
        count,
        "missing native continuation response capture"
    );
    let rounds: Vec<WireRound> = captures
        .iter()
        .map(|capture| {
            assert!(
                !capture.overflow.load(Ordering::SeqCst),
                "native response capture exceeded observation bound"
            );
            wire_round(kind, &capture.bytes.lock().unwrap()).unwrap_or_else(|_| {
                panic!(
                    "native continuation terminal or content invalid; response details suppressed"
                )
            })
        })
        .collect();
    assert!(
        rounds[..count - 1]
            .iter()
            .all(|round| round.stop == WireStop::Length),
        "continued a reply without a native length stop"
    );
    let final_stop = rounds.last().unwrap().stop;
    assert!(
        final_stop == WireStop::Complete
            || (final_stop == WireStop::Length && count == MAX_CONTINUATION_REQUESTS),
        "incomplete reply returned before native continuation limit"
    );
    let joined: String = rounds.iter().map(|round| round.text.as_str()).collect();
    let responses = fixture.witness.responses.lock().unwrap().clone();
    assert!(
        responses.len() == 1 && responses[0].tool_calls.is_empty(),
        "continuation produced extra host responses or tool calls"
    );
    assert!(
        responses[0].text == joined && run.text.as_deref() == Some(joined.as_str()),
        "native round text was not joined into final reply"
    );
    assert!(
        *fixture.witness.text.lock().unwrap() == joined,
        "listener differs from joined native rounds"
    );
    let messages = fixture.stored_messages(&session.id);
    assert!(
        messages.len() == 2 && messages[1].role == Role::Assistant && messages[1].content == joined,
        "joined reply was not persisted as one assistant message"
    );
    let stored_state = messages[1]
        .provider_state
        .as_ref()
        .expect("joined native state not persisted");
    let response_state = responses[0]
        .provider_state
        .as_ref()
        .expect("joined response native state missing");
    assert!(
        serde_json::to_vec(stored_state).unwrap() == serde_json::to_vec(response_state).unwrap(),
        "joined native state changed in SQLite"
    );
    assert!(
        provider_state_replay_compatible(stored_state, kind.adapter(), &fixture.model),
        "joined state origin differs from provider/model"
    );
    let expected: Vec<Value> = rounds
        .iter()
        .flat_map(|round| round.artifacts.iter().cloned())
        .collect();
    assert!(
        serde_json::to_vec(&artifacts(stored_state)).unwrap()
            == serde_json::to_vec(&expected).unwrap(),
        "complete replayable reasoning from native rounds was lost, duplicated or reordered"
    );
    let extra: Vec<Value> = rounds
        .iter()
        .flat_map(|round| round.extra_artifacts.iter().cloned())
        .collect();
    assert!(
        extra_chat_artifacts(stored_state) == extra,
        "opaque streamed reasoning fields were lost, duplicated or reordered"
    );
    if kind.adapter() == "chat-completions" && count > 1 {
        assert!(
            stored_state["rounds"]
                .as_array()
                .is_some_and(|rounds| rounds.len() == count),
            "chat continuation state omitted a native round"
        );
    }
    fixture.provider.close_session(&session.id).await.unwrap();
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WireStop {
    Length,
    Complete,
}

struct WireRound {
    stop: WireStop,
    text: String,
    artifacts: Vec<Value>,
    extra_artifacts: Vec<Value>,
}

// Compare field paths and arrival order, rather than counting equal values
// recursively: OpenRouter can send identical text in reasoning and details.
fn unknown_chat_reasoning(fields: &Value) -> Vec<Value> {
    let mut result = Vec::new();
    if let Some(fields) = fields.as_object() {
        for (name, value) in fields {
            let assembled = match name.as_str() {
                "reasoning" | "reasoning_content" => value.is_string(),
                "reasoning_details" => value.is_array(),
                _ => false,
            };
            if name.starts_with("reasoning") && !assembled && !value.is_null() {
                result.push(json!({name:value}));
            }
        }
    }
    result
}

fn extra_chat_artifacts(state: &Value) -> Vec<Value> {
    let mut result = unknown_chat_reasoning(&state["message_fields"]);
    if let Some(fragments) = state["opaque_stream_fragments"].as_array() {
        for fragment in fragments {
            result.extend(unknown_chat_reasoning(&fragment["choices"][0]["delta"]));
        }
    }
    if let Some(rounds) = state["rounds"].as_array() {
        for round in rounds {
            result.extend(extra_chat_artifacts(round));
        }
    }
    result
}

fn add_wire_event(data: &mut String, events: &mut Vec<Value>) -> Result<()> {
    let event = std::mem::take(data);
    if !event.trim().is_empty() && event.trim() != "[DONE]" {
        events.push(serde_json::from_str(&event)?);
    }
    Ok(())
}

fn wire_events(bytes: &[u8]) -> Result<Vec<Value>> {
    // Support a native JSON fallback as well as SSE, without altering either.
    if let Ok(payload) = serde_json::from_slice::<Value>(bytes) {
        return Ok(vec![payload]);
    }
    let body = std::str::from_utf8(bytes)?;
    let mut events = Vec::new();
    let mut data = String::new();
    for line in body.lines() {
        if line.is_empty() {
            add_wire_event(&mut data, &mut events)?;
        } else if let Some(part) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(part.strip_prefix(' ').unwrap_or(part));
        }
    }
    add_wire_event(&mut data, &mut events)?;
    ensure!(!events.is_empty(), "native response has no JSON events");
    Ok(events)
}

fn append_native_field(block: &mut Value, field: &str, fragment: &str) {
    let mut text = block[field].as_str().unwrap_or("").to_owned();
    text.push_str(fragment);
    block[field] = json!(text);
}

/// Independent wire observation retains the stop reason which Responses/chat
/// intentionally omit from their joined provider state. Unsigned thinking cut
/// by a length stop is not a complete replayable artifact (the Messages adapter
/// deliberately discards it); complete signed/redacted blocks must persist.
fn wire_round(kind: Kind, bytes: &[u8]) -> Result<WireRound> {
    let events = wire_events(bytes)?;
    match kind.adapter() {
        "responses" => {
            let mut terminal = None;
            let mut items = BTreeMap::new();
            let mut streamed = String::new();
            for event in &events {
                match event["type"].as_str() {
                    Some("response.output_text.delta") => {
                        streamed.push_str(event["delta"].as_str().unwrap_or(""))
                    }
                    Some("response.output_item.done") => {
                        items.insert(
                            event["output_index"].as_u64().unwrap_or(items.len() as u64),
                            event["item"].clone(),
                        );
                    }
                    Some("response.completed" | "response.incomplete") => {
                        terminal = Some(event["response"].clone())
                    }
                    _ if event["object"] == "response" => terminal = Some(event.clone()),
                    _ => {}
                }
            }
            let terminal =
                terminal.ok_or_else(|| anyhow::anyhow!("native Responses terminal missing"))?;
            let stop = if terminal["status"] == "incomplete"
                && terminal["incomplete_details"]["reason"] == "max_output_tokens"
            {
                WireStop::Length
            } else {
                ensure!(
                    terminal["status"] == "completed",
                    "native Responses terminal is neither complete nor length"
                );
                WireStop::Complete
            };
            let items: Vec<Value> = terminal["output"]
                .as_array()
                .filter(|items| !items.is_empty())
                .cloned()
                .unwrap_or_else(|| items.into_values().collect());
            let mut text = String::new();
            for item in &items {
                if item["type"] == "message"
                    && let Some(parts) = item["content"].as_array()
                {
                    for part in parts {
                        if part["type"] == "output_text" {
                            text.push_str(part["text"].as_str().unwrap_or(""));
                        }
                    }
                }
            }
            if text.is_empty() {
                text = terminal["output_text"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or(streamed);
            }
            Ok(WireRound {
                stop,
                text,
                artifacts: artifacts(&json!({"items":items})),
                extra_artifacts: Vec::new(),
            })
        }
        "anthropic-messages" => {
            let mut blocks = BTreeMap::new();
            let mut reason = None;
            let mut terminal = false;
            for event in &events {
                match event["type"].as_str() {
                    Some("message") => {
                        if let Some(content) = event["content"].as_array() {
                            for (index, block) in content.iter().enumerate() {
                                blocks.insert(index as u64, block.clone());
                            }
                        }
                        reason = event["stop_reason"].as_str().map(str::to_owned);
                        terminal = true;
                    }
                    Some("content_block_start") => {
                        blocks.insert(
                            event["index"].as_u64().unwrap_or(0),
                            event["content_block"].clone(),
                        );
                    }
                    Some("content_block_delta") => {
                        let block = blocks
                            .entry(event["index"].as_u64().unwrap_or(0))
                            .or_insert_with(|| json!({}));
                        let delta = &event["delta"];
                        let field = match delta["type"].as_str() {
                            Some("text_delta") => Some("text"),
                            Some("thinking_delta") => Some("thinking"),
                            Some("signature_delta") => Some("signature"),
                            _ => None,
                        };
                        if let Some(field) = field {
                            append_native_field(block, field, delta[field].as_str().unwrap_or(""));
                        }
                    }
                    Some("message_delta") if event["delta"]["stop_reason"].is_string() => {
                        reason = event["delta"]["stop_reason"].as_str().map(str::to_owned)
                    }
                    Some("message_stop") => terminal = true,
                    _ => {}
                }
            }
            ensure!(terminal, "native Messages terminal missing");
            let stop = match reason.as_deref() {
                Some("max_tokens") => WireStop::Length,
                Some("end_turn" | "stop_sequence") => WireStop::Complete,
                _ => anyhow::bail!("native Messages terminal is neither complete nor length"),
            };
            let mut text = String::new();
            let mut blocks: Vec<Value> = blocks.into_values().collect();
            for block in &blocks {
                if block["type"] == "text" {
                    text.push_str(block["text"].as_str().unwrap_or(""));
                }
            }
            if stop == WireStop::Length {
                blocks.retain(|block| {
                    block["type"] != "thinking"
                        || block["signature"]
                            .as_str()
                            .is_some_and(|signature| !signature.is_empty())
                });
            }
            Ok(WireRound {
                stop,
                text,
                artifacts: artifacts(&json!({"content":blocks})),
                extra_artifacts: Vec::new(),
            })
        }
        "chat-completions" => {
            let mut text = String::new();
            let mut reasoning = String::new();
            let mut plain_reasoning = String::new();
            let mut reason = None;
            let mut extra_artifacts = Vec::new();
            let mut native_details = Vec::new();
            for event in &events {
                let choice = &event["choices"][0];
                if let Some(finish) = choice["finish_reason"].as_str() {
                    reason = Some(finish.to_owned());
                }
                let fields = choice.get("delta").unwrap_or(&choice["message"]);
                text.push_str(fields["content"].as_str().unwrap_or(""));
                reasoning.push_str(fields["reasoning_content"].as_str().unwrap_or(""));
                plain_reasoning.push_str(fields["reasoning"].as_str().unwrap_or(""));
                if let Some(details) = fields["reasoning_details"].as_array() {
                    native_details.extend(details.iter().cloned());
                }
                extra_artifacts.extend(unknown_chat_reasoning(fields));
            }
            let stop = match reason.as_deref() {
                Some("length") => WireStop::Length,
                Some("stop") => WireStop::Complete,
                _ => anyhow::bail!("native chat terminal is neither complete nor length"),
            };
            let mut native_artifacts = if reasoning.is_empty() {
                Vec::new()
            } else {
                vec![json!(reasoning)]
            };
            if !plain_reasoning.is_empty() {
                native_artifacts.push(json!(plain_reasoning));
            }
            native_artifacts.extend(native_details);
            Ok(WireRound {
                stop,
                text,
                artifacts: native_artifacts,
                extra_artifacts,
            })
        }
        _ => anyhow::bail!("continuation wire observation is HTTP-only"),
    }
}

#[test]
fn openrouter_wire_observation_preserves_parallel_reasoning_without_false_duplicates() {
    // OpenRouter documents details as an ordered sequence across delta chunks.
    // Plain reasoning can contain exactly the same text as a details object.
    let detail = json!({
        "type":"reasoning.text", "text":"Think π", "signature":null,
        "id":"reasoning-text-1", "format":"anthropic-claude-v1", "index":0
    });
    let bytes = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{
            "reasoning":"Think π", "reasoning_details":[detail.clone()],
            "reasoning_future":{"text":"Think π"}
        }}]}),
        json!({"choices":[{"delta":{"content":"answer"},"finish_reason":"length"}]})
    );
    let round = wire_round(Kind::OpenrouterChat, bytes.as_bytes()).unwrap();
    assert!(round.stop == WireStop::Length);
    assert_eq!(round.text, "answer");
    assert_eq!(round.artifacts, vec![json!("Think π"), detail.clone()]);
    let state = json!({"message_fields":{
        "reasoning":"Think π", "reasoning_details":[detail]
    },"opaque_stream_fragments":[{"choices":[{"delta":{
        "reasoning_future":{"text":"Think π"}
    }}]}]});
    assert_eq!(artifacts(&state), round.artifacts);
    assert_eq!(extra_chat_artifacts(&state), round.extra_artifacts);
    let duplicated = json!({"rounds":[state.clone(),state]});
    assert_ne!(artifacts(&duplicated), round.artifacts);
    assert_ne!(extra_chat_artifacts(&duplicated), round.extra_artifacts);
}

macro_rules! live_kind {
    ($module:ident, $kind:ident) => {
        mod $module {
            use super::*;

            #[tokio::test]
            #[ignore = "live: paid model calls"]
            async fn stream() {
                super::stream(Kind::$kind).await;
            }

            #[tokio::test]
            #[ignore = "live: paid model calls"]
            async fn tools() {
                super::tools(Kind::$kind).await;
            }

            #[tokio::test]
            #[ignore = "live: paid model calls"]
            async fn cancel() {
                super::cancel(Kind::$kind).await;
            }

            #[tokio::test]
            #[ignore = "live: paid model calls"]
            async fn continuation() {
                super::continuation(Kind::$kind).await;
            }

            #[tokio::test]
            #[ignore = "live: paid model calls"]
            async fn schema() {
                super::schema(Kind::$kind).await;
            }

            #[tokio::test]
            #[ignore = "live: paid model calls"]
            async fn bad_key() {
                super::bad_key(Kind::$kind).await;
            }
        }
    };
}

live_kind!(codex, Codex);
live_kind!(responses, Responses);
live_kind!(anthropic_messages, AnthropicMessages);
live_kind!(opencode_messages, OpencodeMessages);
live_kind!(opencode_chat, OpencodeChat);
live_kind!(openrouter_chat, OpenrouterChat);
live_kind!(deepseek_chat, DeepseekChat);
live_kind!(claude_code, ClaudeCode);
