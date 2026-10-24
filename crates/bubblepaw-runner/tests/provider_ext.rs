//! Provider extensions: tool surface, registry routing and the out-of-process host.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Capabilities, EventKind, ExecutionMode, Harness, HarnessConfig,
    HistoryCursor, Message, ModelRequest, ModelResponse, Provider, ProviderError,
    ProviderErrorKind, Role, RunOptions, Store, TextCallback, Tool, ToolCall, ToolContext,
    ToolDefinition, ToolRegistry, ToolResult, ToolSurface,
};
use bubblepaw_runner::{
    artifacts::ArtifactStore,
    codemode::CodeMode,
    provider::{ExtensionConfig, ExtensionProvider, ProviderRegistry, load_extensions},
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Named(&'static str);
#[async_trait]
impl Tool for Named {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            approval: Default::default(),
            name: self.0.into(),
            description: "test tool".into(),
            read_only: true,
            input_schema: json!({"type":"object"}),
        }
    }
    async fn invoke(&self, _: ToolContext, input: Value) -> Result<ToolResult> {
        Ok(ToolResult::text(
            json!({"tool":self.0,"value":input["value"]}).to_string(),
        ))
    }
}

/// Calls `js` once and records what it was offered.
struct SurfaceProvider {
    surface: ToolSurface,
    offered: Mutex<Vec<Vec<String>>>,
}
#[async_trait]
impl Provider for SurfaceProvider {
    fn capabilities(&self, _: &str) -> Capabilities {
        Capabilities {
            tool_surface: self.surface.clone(),
            ..Default::default()
        }
    }
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.offered
            .lock()
            .unwrap()
            .push(request.tools.iter().map(|t| t.name.clone()).collect());
        if request.messages.last().unwrap().role == Role::User {
            return Ok(ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "js".into(),
                    arguments: json!({"code":"return await tools.host_echo({value:7})"}),
                }],
                ..Default::default()
            });
        }
        Ok(ModelResponse {
            text: "done".into(),
            ..Default::default()
        })
    }
}

async fn run_surface(surface: ToolSurface) -> (Vec<Vec<String>>, Message) {
    let root = tempfile::tempdir().unwrap();
    let artifacts =
        Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap());
    let mode = Arc::new(CodeMode::new(root.path().to_owned(), artifacts).unwrap());
    let mut tools = ToolRegistry::new();
    tools.register_host(Arc::new(Named("host_echo"))).unwrap();
    tools.register(Arc::new(Named("other"))).unwrap();
    mode.register(&mut tools).unwrap();
    let store = Store::in_memory().unwrap();
    let session = store.create_session("surface").unwrap().id;
    let provider = Arc::new(SurfaceProvider {
        surface,
        offered: Mutex::default(),
    });
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        tools,
        HarnessConfig {
            execution_mode: ExecutionMode::Js,
            ..Default::default()
        },
    );
    harness
        .prompt(&session, "go", RunOptions::default())
        .await
        .unwrap();
    let tool = store
        .messages(&session)
        .unwrap()
        .into_iter()
        .find(|m| m.role == Role::Tool)
        .unwrap();
    let offered = provider.offered.lock().unwrap().clone();
    (offered, tool)
}

#[tokio::test]
async fn code_surface_offers_only_js_and_deferred_tools_run_inside_it() {
    let (offered, tool) = run_surface(ToolSurface::Code).await;
    assert_eq!(offered[0], ["js"]);
    assert_eq!(offered[1], ["js"]);
    assert!(!tool.is_error, "{}", tool.content);
    assert!(tool.content.contains("host_echo"), "{}", tool.content);
    assert!(tool.content.contains('7'), "{}", tool.content);
    let (offered, _) = run_surface(ToolSurface::Direct).await;
    assert_eq!(
        offered[0],
        ["js"],
        "JS mode is exclusive even for a direct-capable provider"
    );
}

/// Records forwarded requests and closed sessions.
#[derive(Default)]
struct Probe {
    requests: Mutex<Vec<ModelRequest>>,
    closed: Mutex<Vec<String>>,
}
#[async_trait]
impl Provider for Probe {
    fn models(&self) -> Vec<bubblepaw_core::ModelInfo> {
        vec![bubblepaw_core::ModelInfo {
            id: "m1".into(),
            name: "M1".into(),
        }]
    }
    fn capabilities(&self, model: &str) -> Capabilities {
        Capabilities {
            context_window: Some(model.len()),
            ..Default::default()
        }
    }
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        Ok(ModelResponse::default())
    }
    async fn close_session(&self, session_id: &str) -> Result<()> {
        self.closed.lock().unwrap().push(session_id.into());
        Ok(())
    }
}
fn step(model: &str, session: &str, epoch: &str, rewritten: Option<&str>) -> ModelRequest {
    ModelRequest {
        model: model.into(),
        session_id: session.into(),
        history: HistoryCursor {
            epoch: epoch.into(),
            rewritten_from: rewritten.map(Into::into),
        },
        ..Default::default()
    }
}

#[tokio::test]
async fn registry_routes_by_prefix_and_resets_the_cursor_on_switch() {
    let codex = Arc::new(Probe::default());
    let other = Arc::new(Probe::default());
    let mut registry = ProviderRegistry::new("codex", codex.clone());
    registry.insert("other", other.clone()).unwrap();
    assert!(registry.insert("other", other.clone()).is_err());
    let ids: Vec<_> = registry.models().into_iter().map(|m| m.id).collect();
    assert_eq!(ids, ["codex/m1", "other/m1"]);
    assert_eq!(registry.capabilities("other/abc").context_window, Some(3));
    assert_eq!(
        registry.capabilities("unknown/abcdef").context_window,
        Some(14)
    );
    let noop: TextCallback = Arc::new(|_| {});
    let token = CancellationToken::new();
    // Default fallback, unchanged model, including an unknown prefix.
    registry
        .generate(step("gpt-x", "s", "e1", None), noop.clone(), token.clone())
        .await
        .unwrap();
    registry
        .generate(
            step("vendor/gpt-x", "s", "e1", None),
            noop.clone(),
            token.clone(),
        )
        .await
        .unwrap();
    // Explicit prefix strips; the same provider keeps the cursor.
    registry
        .generate(
            step("codex/gpt-y", "s", "e1", None),
            noop.clone(),
            token.clone(),
        )
        .await
        .unwrap();
    {
        let requests = codex.requests.lock().unwrap();
        let models: Vec<_> = requests.iter().map(|r| r.model.as_str()).collect();
        assert_eq!(models, ["gpt-x", "vendor/gpt-x", "gpt-y"]);
        assert!(requests.iter().all(|r| r.history.epoch == "e1"));
    }
    assert!(
        codex.closed.lock().unwrap().is_empty(),
        "same-provider model changes must not dispose the provider mirror"
    );
    // Switching the session to another provider closes it on the old one and
    // sends a fresh cursor; later appends keep that fresh epoch.
    registry
        .generate(
            step("other/m1", "s", "e1", Some("x")),
            noop.clone(),
            token.clone(),
        )
        .await
        .unwrap();
    registry
        .generate(
            step("other/m1", "s", "e1", None),
            noop.clone(),
            token.clone(),
        )
        .await
        .unwrap();
    assert_eq!(*codex.closed.lock().unwrap(), ["s"]);
    assert!(other.closed.lock().unwrap().is_empty());
    {
        let requests = other.requests.lock().unwrap();
        assert_eq!(requests[0].model, "m1");
        assert_ne!(requests[0].history.epoch, "e1");
        assert_eq!(requests[0].history.rewritten_from, None);
        assert_eq!(requests[1].history.epoch, requests[0].history.epoch);
    }
    // Another session on the old provider is unaffected.
    registry
        .generate(
            step("gpt-x", "t", "e9", Some("m")),
            noop.clone(),
            token.clone(),
        )
        .await
        .unwrap();
    assert_eq!(
        codex.requests.lock().unwrap().last().unwrap().history,
        HistoryCursor {
            epoch: "e9".into(),
            rewritten_from: Some("m".into())
        }
    );
    registry.close_session("s").await.unwrap();
    assert_eq!(*codex.closed.lock().unwrap(), ["s", "s"]);
    assert_eq!(*other.closed.lock().unwrap(), ["s"]);
}

fn fake() -> ExtensionProvider {
    fake_with_capabilities(None)
}
fn fake_with_capabilities(capabilities: Option<Value>) -> ExtensionProvider {
    ExtensionProvider::new(ExtensionConfig {
        id: "fake".into(),
        command: env!("CARGO_BIN_EXE_bubblepaw-fake-extension").into(),
        args: capabilities
            .map(|value| vec![format!("--capabilities={value}")])
            .unwrap_or_default(),
        cwd: None,
        ..Default::default()
    })
}
fn user(content: &str) -> Message {
    Message {
        id: uuid::Uuid::new_v4().to_string(),
        session_id: "s".into(),
        role: Role::User,
        origin: None,
        content: content.into(),
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: "2026-10-08T00:00:00Z".into(),
    }
}
fn ext_step(model: &str, content: &str, epoch: &str, rewritten: Option<&str>) -> ModelRequest {
    ModelRequest {
        messages: vec![user(content)],
        ..step(model, "s", epoch, rewritten)
    }
}
async fn call(provider: &ExtensionProvider, request: ModelRequest) -> Result<ModelResponse> {
    provider
        .generate(request, Arc::new(|_| {}), CancellationToken::new())
        .await
}
#[path = "provider_ext/in_flight.rs"]
mod in_flight;
#[path = "provider_ext/submission.rs"]
mod submission;

#[tokio::test]
async fn extension_pre_cancelled_request_never_initializes_or_reaches_step() {
    let provider = fake();
    let token = CancellationToken::new();
    token.cancel();
    for initialized in [false, true] {
        if initialized {
            provider.start().await.unwrap();
        }
        let error = provider
            .generate(
                ext_step("direct-model", "hello", "e1", None),
                Arc::new(|_| {}),
                token.clone(),
            )
            .await
            .unwrap_err();
        let typed = ProviderError::of(&error).unwrap();
        assert_eq!(typed.kind, ProviderErrorKind::Transient);
        assert_eq!(typed.message, "cancelled");
        if !initialized {
            assert!(provider.models().is_empty(), "extension was initialized");
        }
    }
    let state = call(&provider, ext_step("direct-model", "hello", "e1", None))
        .await
        .unwrap()
        .provider_state
        .unwrap();
    assert_eq!(state["step_count"], 1, "pre-cancelled request reached step");
}

#[tokio::test]
async fn extension_initializes_streams_and_echoes_the_cursor() {
    let provider = fake();
    assert!(provider.models().is_empty());
    provider.start().await.unwrap();
    let models: Vec<_> = provider.models().into_iter().map(|m| m.id).collect();
    assert_eq!(models, ["code-model", "direct-model"]);
    let capabilities = provider.capabilities("code-model");
    assert_eq!(capabilities.context_window, Some(200_000));
    assert_eq!(capabilities.cache, bubblepaw_core::CacheMode::Session);
    assert!(capabilities.stateful);
    assert_eq!(capabilities.tool_surface, ToolSurface::Code);
    assert_eq!(
        provider.capabilities("direct-model").tool_surface,
        ToolSurface::Direct
    );
    let streamed = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = streamed.clone();
    let response = provider
        .generate(
            ext_step("direct-model", "text Hello|, |world", "e1", Some("m0")),
            Arc::new(move |text| sink.lock().unwrap().push(text.into())),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.text, "Hello, world");
    assert_eq!(streamed.lock().unwrap().concat(), "Hello, world");
    assert!(streamed.lock().unwrap().len() < 3);
    assert_eq!(response.usage.unwrap().cache_read_tokens, 3);
    let state = response.provider_state.unwrap();
    assert_eq!(
        state["history"],
        json!({"epoch":"e1","rewritten_from":"m0"})
    );
    assert_eq!(state["session_id"], "s");
    assert_eq!(state["model"], "direct-model");
}

#[tokio::test]
async fn extension_forwards_the_callers_selected_tool_surface() {
    let provider = fake();
    let tool = |name: &str| ToolDefinition {
        approval: Default::default(),
        name: name.into(),
        description: String::new(),
        input_schema: json!({"type":"object"}),
        read_only: true,
    };
    let mut request = ext_step("code-model", "hello", "e1", None);
    request.tools = vec![tool("history_search"), tool("js"), tool("other")];
    let response = call(&provider, request).await.unwrap();
    assert_eq!(
        response.provider_state.unwrap()["tools"],
        json!(["history_search", "js", "other"])
    );
}

#[tokio::test]
async fn extension_tool_calls_round_trip_through_the_harness() {
    let store = Store::in_memory().unwrap();
    let session = store.create_session("ext").unwrap().id;
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Named("echo"))).unwrap();
    let mut registry = ProviderRegistry::new("demo", Arc::new(Probe::default()));
    registry.insert("fake", Arc::new(fake())).unwrap();
    let harness = Harness::new(
        store.clone(),
        Arc::new(registry),
        tools,
        HarnessConfig {
            model: "fake/direct-model".into(),
            ..HarnessConfig::default()
        },
    );
    let run = harness
        .prompt(&session, "call", RunOptions::default())
        .await
        .unwrap();
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(
        run.text.as_deref(),
        Some(r#"tool result: {"tool":"echo","value":"hi"}"#)
    );
    let messages = store.messages(&session).unwrap();
    assert_eq!(messages[1].tool_calls[0].name, "echo");
    let first = messages[1].provider_state.as_ref().unwrap();
    let second = messages[3].provider_state.as_ref().unwrap();
    assert_eq!(first["session_id"], session.as_str());
    assert_eq!(first["model"], "direct-model");
    assert_eq!(first["history"]["epoch"], second["history"]["epoch"]);
    assert_eq!(second["history"]["rewritten_from"], Value::Null);
}

#[tokio::test]
async fn extension_error_kinds_map() {
    let provider = fake();
    for (name, kind, message) in [
        (
            "auth",
            ProviderErrorKind::Auth,
            "extension authentication failed",
        ),
        (
            "rate_limit",
            ProviderErrorKind::RateLimit {
                resets_at: Some("2026-10-08T14:00:00Z".into()),
            },
            "extension rate limit reached",
        ),
        (
            "context",
            ProviderErrorKind::Context,
            "extension context limit exceeded",
        ),
        (
            "transient",
            ProviderErrorKind::Transient,
            "extension request failed transiently",
        ),
        (
            "fatal",
            ProviderErrorKind::Fatal,
            "extension request failed",
        ),
    ] {
        let (result, rows) = bubblepaw_core::capture_request_usage(
            "extension-session",
            Some("extension-run"),
            bubblepaw_core::RequestKind::Main,
            call(
                &provider,
                ext_step("direct-model", &format!("error {name}"), "e1", None),
            ),
        )
        .await;
        let error = result.unwrap_err();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].failed);
        assert_eq!(rows[0].provider, "fake");
        assert_eq!(rows[0].session_id, "extension-session");
        let typed = ProviderError::of(&error).expect("typed error");
        assert_eq!(typed.kind, kind);
        assert_eq!(typed.message, message);
    }
}

#[tokio::test]
async fn extension_batches_many_tiny_unicode_fragments_without_losing_text() {
    let provider = fake();
    let pieces = vec!["猫🦀é"; 600];
    let streamed = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = streamed.clone();
    let response = provider
        .generate(
            ext_step(
                "direct-model",
                &format!("text {}", pieces.join("|")),
                "e1",
                None,
            ),
            Arc::new(move |text| sink.lock().unwrap().push(text.into())),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let batches = streamed.lock().unwrap();
    assert_eq!(batches.concat(), pieces.concat());
    assert_eq!(response.text, pieces.concat());
    assert!(
        batches.len() < pieces.len() / 10,
        "{} callbacks",
        batches.len()
    );
}

#[tokio::test]
async fn extension_flushes_pending_text_while_the_step_is_still_open() {
    let provider = Arc::new(fake());
    provider.start().await.unwrap();
    let (published, mut listener) = tokio::sync::mpsc::unbounded_channel::<String>();
    let task = tokio::spawn(async move {
        provider
            .generate(
                ext_step("direct-model", "text-delay 2000 猫🦀", "e1", None),
                Arc::new(move |text| {
                    let _ = published.send(text.into());
                }),
                CancellationToken::new(),
            )
            .await
    });
    let text = tokio::time::timeout(Duration::from_secs(1), listener.recv())
        .await
        .expect("pending text should flush before the delayed reply")
        .unwrap();
    assert_eq!(text, "猫🦀");
    assert!(
        !task.is_finished(),
        "text arrived only at the terminal reply"
    );
    assert_eq!(task.await.unwrap().unwrap().text, "猫🦀");
}

#[tokio::test]
async fn extension_error_flushes_queued_text_and_does_not_retry_the_step() {
    let provider = fake();
    let streamed = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = streamed.clone();
    let error = provider
        .generate(
            ext_step("direct-model", "text-error 尾|猫|🦀", "e1", None),
            Arc::new(move |text| sink.lock().unwrap().push(text.into())),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(streamed.lock().unwrap().concat(), "尾猫🦀");
    let typed = ProviderError::of(&error).unwrap();
    assert_eq!(typed.kind, ProviderErrorKind::Transient);
    assert_eq!(typed.message, "extension request failed transiently");
    assert!(typed.published);
    assert!(!bubblepaw_core::provider_error_retryable(&error));
    assert!(!format!("{error:#}").contains("RAW_RPC_ERROR_SENTINEL"));
    let state = call(&provider, ext_step("direct-model", "hello", "e1", None))
        .await
        .unwrap()
        .provider_state
        .unwrap();
    assert_eq!(state["step_count"], 2, "failed step was retried");
}

#[tokio::test]
async fn extension_concurrent_text_batches_stay_with_their_request() {
    let provider = fake();
    let left = Arc::new(Mutex::new(String::new()));
    let right = Arc::new(Mutex::new(String::new()));
    let left_sink = left.clone();
    let right_sink = right.clone();
    let (a, b) = tokio::join!(
        provider.generate(
            ext_step("direct-model", "text-delay 150 左|猫", "e1", None),
            Arc::new(move |text| left_sink.lock().unwrap().push_str(text)),
            CancellationToken::new(),
        ),
        provider.generate(
            ext_step("direct-model", "text-delay 150 右|🦀", "e1", None),
            Arc::new(move |text| right_sink.lock().unwrap().push_str(text)),
            CancellationToken::new(),
        ),
    );
    assert_eq!(a.unwrap().text, "左猫");
    assert_eq!(b.unwrap().text, "右🦀");
    assert_eq!(*left.lock().unwrap(), "左猫");
    assert_eq!(*right.lock().unwrap(), "右🦀");
}

#[tokio::test]
async fn extension_rpc_error_messages_hide_raw_text_and_keep_option_names() {
    let provider = fake();
    for (command, expected) in [
        (
            "raw-error auth|RAW_RPC_ERROR_SENTINEL",
            "extension authentication failed",
        ),
        (
            "raw-error rate_limit|RAW_RPC_ERROR_SENTINEL",
            "extension rate limit reached",
        ),
        (
            "raw-error context|RAW_RPC_ERROR_SENTINEL",
            "extension context limit exceeded",
        ),
        (
            "raw-error transient|RAW_RPC_ERROR_SENTINEL",
            "extension request failed transiently",
        ),
        (
            "raw-error fatal|RAW_RPC_ERROR_SENTINEL",
            "extension request failed",
        ),
        (
            "option-error output_schema|RAW_RPC_ERROR_SENTINEL",
            "extension rejected output_schema",
        ),
        (
            "option-error reasoning_effort|RAW_RPC_ERROR_SENTINEL",
            "extension rejected reasoning_effort",
        ),
        ("raw-error transient|cancelled", "cancelled"),
    ] {
        let error = call(&provider, ext_step("direct-model", command, "e1", None))
            .await
            .unwrap_err();
        let typed = ProviderError::of(&error).unwrap();
        assert_eq!(typed.message, expected);
        assert!(!format!("{error:#}").contains("RAW_RPC_ERROR_SENTINEL"));
        if command.contains("rate_limit") {
            assert_eq!(
                typed.kind,
                ProviderErrorKind::RateLimit {
                    resets_at: Some("2026-10-08T14:00:00Z".into()),
                }
            );
        }
    }
}

#[tokio::test]
async fn extension_process_errors_do_not_include_child_stderr() {
    let provider = fake();
    let error = call(
        &provider,
        ext_step(
            "direct-model",
            "stderr-exit RAW_STDERR_SENTINEL",
            "e1",
            None,
        ),
    )
    .await
    .unwrap_err();
    let typed = ProviderError::of(&error).unwrap();
    assert_eq!(typed.kind, ProviderErrorKind::Transient);
    assert_eq!(typed.message, "extension fake exited");
    assert!(!format!("{error:#}").contains("RAW_STDERR_SENTINEL"));
}

#[tokio::test]
async fn extension_invalid_step_error_does_not_include_raw_child_values() {
    let provider = fake();
    let error = call(
        &provider,
        ext_step("direct-model", "invalid-step", "e1", None),
    )
    .await
    .unwrap_err();
    let typed = ProviderError::of(&error).unwrap();
    assert_eq!(typed.kind, ProviderErrorKind::Fatal);
    assert_eq!(typed.message, "extension fake sent an invalid step result");
    assert!(!format!("{error:#}").contains("RAW_INVALID_STEP_SENTINEL"));
}

struct CountedEcho(Arc<AtomicUsize>);
#[async_trait]
impl Tool for CountedEcho {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            approval: Default::default(),
            name: "echo".into(),
            description: "counted effect".into(),
            input_schema: json!({
                "type":"object",
                "properties":{"value":{"type":"string"}},
                "required":["value"],
                "additionalProperties":false
            }),
            read_only: false,
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::text("should never execute"))
    }
}

#[tokio::test]
async fn extension_invalid_tool_arguments_are_metered_and_durably_rejected_without_effects()
-> Result<()> {
    for (mode, arguments) in [("null", Value::Null), ("schema", json!({"value":17}))] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("rejected.db");
        let store = Store::open(&path)?;
        let session = store.create_session("rejected extension call")?.id;
        let invocations = Arc::new(AtomicUsize::new(0));
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(CountedEcho(invocations.clone())))?;
        let harness = Harness::new(
            store.clone(),
            Arc::new(fake()),
            tools,
            HarnessConfig {
                model: "direct-model".into(),
                ..Default::default()
            },
        );
        let run = harness
            .prompt(&session, format!("bad-call {mode}"), RunOptions::default())
            .await?;
        assert_eq!(run.status, "completed", "{:?}", run.error);
        assert_eq!(invocations.load(Ordering::SeqCst), 0);
        assert!(store.pending_effects(&session)?.is_empty());
        let chat_path = store.chat_path(&session)?.expect("file-backed chat");
        let effects: i64 = rusqlite::Connection::open(chat_path)?.query_row(
            "SELECT COUNT(*) FROM effects",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(effects, 0, "invalid call crossed the dispatch boundary");
        let usage = run.usage.unwrap();
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 4);
        assert_eq!(usage.cache_read_tokens, 6);
        let rows = store.request_usage()?;
        assert_eq!(rows.len(), 2, "one row per extension step");
        let mut billed = bubblepaw_core::Usage::default();
        for row in &rows {
            assert_eq!(row.provider, "fake");
            assert_eq!(row.model, "direct-model");
            assert_eq!(row.session_id, session);
            assert_eq!(row.run_id.as_deref(), Some(run.id.as_str()));
            assert!(!row.failed);
            billed.add(&row.usage);
        }
        assert_eq!(billed, usage);
        let messages = store.messages(&session)?;
        let call = &messages
            .iter()
            .find(|m| !m.tool_calls.is_empty())
            .unwrap()
            .tool_calls[0];
        assert_eq!(call.id, "bad-extension-call");
        assert_eq!(call.arguments, arguments);
        let rejected = messages.iter().find(|m| m.role == Role::Tool).unwrap();
        assert_eq!(rejected.tool_call_id.as_deref(), Some("bad-extension-call"));
        assert!(rejected.is_error);
        let events = harness.events_after(0, Some(&session), 100)?;
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(&event.kind,
                    EventKind::ToolStarted { call } if call.id == "bad-extension-call"
                ))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(&event.kind,
                    EventKind::ToolFinished { call_id, result, .. }
                        if call_id == "bad-extension-call" && result.is_error
                ))
                .count(),
            1
        );
    }
    Ok(())
}

#[tokio::test]
async fn extension_runs_concurrent_steps_and_closes_sessions() {
    let provider = Arc::new(fake());
    provider.start().await.unwrap();
    let slow = {
        let provider = provider.clone();
        tokio::spawn(async move {
            call(&provider, ext_step("direct-model", "sleep 300", "e1", None)).await
        })
    };
    let fast = {
        let provider = provider.clone();
        tokio::spawn(async move {
            call(&provider, ext_step("direct-model", "sleep 10", "e1", None)).await
        })
    };
    let started = std::time::Instant::now();
    let fast = fast.await.unwrap().unwrap();
    let fast_done = started.elapsed();
    let slow = slow.await.unwrap().unwrap();
    assert_eq!(fast.text, "slept 10");
    assert_eq!(slow.text, "slept 300");
    assert!(fast_done < Duration::from_millis(300), "{fast_done:?}");
    provider.close_session("s").await.unwrap();
    let state = call(&provider, ext_step("direct-model", "x", "e1", None))
        .await
        .unwrap()
        .provider_state
        .unwrap();
    assert_eq!(state["closed"], json!(["s"]));
}

#[test]
fn extensions_file_resolves_relative_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("extensions.json");
    std::fs::write(
        &path,
        r#"[{"id":"claude-code","command":"bun","args":["run","main.ts"],"cwd":"ext"},{"id":"b","command":"b"}]"#,
    )
    .unwrap();
    let entries = load_extensions(&path).unwrap();
    assert_eq!(
        entries[0].cwd.as_deref(),
        Some(dir.path().join("ext").as_path())
    );
    assert_eq!(entries[0].args, ["run", "main.ts"]);
    assert!(entries[1].cwd.is_none() && entries[1].args.is_empty());
}

fn fake_with_env(id: &str, env: &[(&str, &str)]) -> ExtensionProvider {
    ExtensionProvider::new(ExtensionConfig {
        id: id.into(),
        command: env!("CARGO_BIN_EXE_bubblepaw-fake-extension").into(),
        env: env
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
        ..Default::default()
    })
}

#[tokio::test]
async fn extension_instance_env_reaches_the_child() {
    let provider = fake_with_env(
        "fake",
        &[
            ("BP_FAKE_ENV", "account-b"),
            ("CLAUDE_CONFIG_DIR", "/home/user/.claude-b"),
        ],
    );
    let response = call(&provider, ext_step("direct-model", "hi", "e1", None))
        .await
        .unwrap();
    let env = &response.provider_state.as_ref().unwrap()["env"];
    assert_eq!(env["BP_FAKE_ENV"], "account-b");
    assert_eq!(
        env["BUBBLEPAW_EXTENSION_ENV_KEYS"],
        "BP_FAKE_ENV,CLAUDE_CONFIG_DIR"
    );
}

#[tokio::test]
async fn usage_limit_on_instance_a_falls_over_to_instance_b() {
    let mut registry = ProviderRegistry::new("demo", Arc::new(Probe::default()));
    registry
        .insert(
            "cc-a",
            Arc::new(fake_with_env("cc-a", &[("BP_FAKE_FAIL", "usage_limit")])),
        )
        .unwrap();
    registry
        .insert(
            "cc-b",
            Arc::new(fake_with_env("cc-b", &[("BP_FAKE_ENV", "b")])),
        )
        .unwrap();
    let limit = "extension usage limit reached: five_hour, resets 2026-10-19T18:00:00Z";
    // Instance A alone reports a distinguishable subscription limit.
    let error = registry
        .generate(
            ext_step("cc-a/direct-model", "hi", "e1", None),
            Arc::new(|_| {}),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    let typed = ProviderError::of(&error).expect("typed error");
    assert_eq!(
        typed.kind,
        ProviderErrorKind::RateLimit {
            resets_at: Some("2026-10-19T18:00:00Z".into())
        }
    );
    assert_eq!(typed.message, limit);
    registry
        .set_model_fallbacks("cc-a/direct-model".into(), vec!["cc-b/direct-model".into()])
        .unwrap();
    let response = registry
        .generate(
            ext_step("cc-a/direct-model", "hi", "e2", None),
            Arc::new(|_| {}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.text, "ok");
    let state = response.provider_state.as_ref().unwrap();
    assert_eq!(state["env"]["BP_FAKE_ENV"], "b");
    assert_eq!(state["_bubblepaw_selected_model"], "cc-b/direct-model");
    assert_eq!(
        state["_bubblepaw_fallback_from"],
        json!(["cc-a/direct-model"])
    );
    assert_eq!(state["_bubblepaw_fallback_reasons"], json!([limit]));
}

/// Usage rows are named by the full model ID the SDK reports; the configured alias
/// only names the request. With that ID the row prices against the existing table.
#[tokio::test]
async fn reported_full_model_id_names_the_usage_row_and_prices_it() {
    let provider = fake_with_env(
        "claude-code",
        &[("BP_FAKE_REPORTED_MODEL", "claude-haiku-5-5")],
    );
    let (result, rows) = bubblepaw_core::capture_request_usage(
        "alias-session",
        Some("alias-run"),
        bubblepaw_core::RequestKind::Main,
        call(&provider, ext_step("haiku", "hi", "e1", None)),
    )
    .await;
    result.unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.provider, "claude-code");
    assert_eq!(row.model, "claude-haiku-5-5");
    assert!(!row.failed);
    assert_eq!(row.usage.output_tokens, 2);
    let at = bubblepaw_runner::pricing::instant(&row.created_at);
    assert!(
        bubblepaw_runner::pricing::PriceTable::shared()
            .usage_cost(&row.model, &row.usage, at)
            .is_some()
    );
}

/// With no reported model the configured alias names the row, which has no price.
#[tokio::test]
async fn configured_alias_is_the_fallback_when_no_model_is_reported() {
    let provider = fake_with_env("claude-code", &[]);
    let (result, rows) = bubblepaw_core::capture_request_usage(
        "alias-session",
        Some("alias-run"),
        bubblepaw_core::RequestKind::Main,
        call(&provider, ext_step("haiku", "hi", "e1", None)),
    )
    .await;
    result.unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.model, "haiku");
    let at = bubblepaw_runner::pricing::instant(&row.created_at);
    assert!(
        bubblepaw_runner::pricing::PriceTable::shared()
            .usage_cost(&row.model, &row.usage, at)
            .is_none()
    );
}

/// A failed call still records the usage the SDK reported, under the reported model.
#[tokio::test]
async fn failed_call_keeps_reported_usage_and_the_reported_model() {
    let provider = fake_with_env(
        "claude-code",
        &[("BP_FAKE_REPORTED_MODEL", "claude-haiku-5-5")],
    );
    let (result, rows) = bubblepaw_core::capture_request_usage(
        "alias-session",
        Some("alias-run"),
        bubblepaw_core::RequestKind::Main,
        call(
            &provider,
            ext_step("haiku", "usage-error transient", "e1", None),
        ),
    )
    .await;
    let error = result.unwrap_err();
    let typed = ProviderError::of(&error).expect("typed error");
    assert_eq!(typed.kind, ProviderErrorKind::Transient);
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert!(row.failed);
    assert_eq!(row.model, "claude-haiku-5-5");
    assert_eq!(row.usage.input_tokens, 5);
    assert_eq!(row.usage.output_tokens, 2);
    assert_eq!(row.usage.cache_read_tokens, 3);
    let at = bubblepaw_runner::pricing::instant(&row.created_at);
    assert!(
        bubblepaw_runner::pricing::PriceTable::shared()
            .usage_cost(&row.model, &row.usage, at)
            .is_some()
    );
}

#[path = "provider_ext/execution_modes.rs"]
mod execution_modes;
