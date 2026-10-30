#![cfg(feature = "web")]

use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, Message, ModelRequest, ModelResponse, Provider,
    Role, RunOptions, TextCallback, Tool, ToolCall, ToolContext, ToolDefinition, ToolRegistry,
    ToolResult, Usage,
};
use bubblepaw_runner::{
    artifacts::ArtifactStore,
    codemode::CodeMode,
    config::{EffectiveConfig, LoadOptions},
    episode_background::EpisodeBackground,
    episode_models::{
        ChapterInput, ChapterResult, EpisodeModels, SplitInput, SplitResult, SummaryInput,
        SummaryResult,
    },
    episode_worker::EpisodeWorker,
    provider::{ScriptedProvider, ScriptedStep},
    summarization::{SummarizingContext, SummaryConfig},
    tools::register_native_tools,
    web::{WebConfig, WebServices, serve_with_services},
    web_providers::{ProviderSetup, router as provider_router},
};
use futures_util::StreamExt;
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{Semaphore, oneshot},
    task::JoinHandle,
};

// The transport fixtures run with the same always-on worker wiring as the CLI.
// All model work stays in process and scripted.
struct WebEpisodeModels;
#[async_trait]
impl EpisodeModels for WebEpisodeModels {
    async fn decide(&self, input: SplitInput) -> Result<SplitResult> {
        let mut probabilities = vec![0.0; input.options.len()];
        probabilities.push(1.0);
        Ok(SplitResult {
            choice: None,
            probabilities,
            model: "fake-decider".into(),
            usage: Usage::default(),
            reasoning: None,
        })
    }
    async fn summarize(&self, _: SummaryInput) -> Result<SummaryResult> {
        Ok(SummaryResult {
            title: "Web episode".into(),
            summary: "# Web episode\nScripted transport facts.".into(),
            model: "fake-summary".into(),
            usage: Usage::default(),
            reasoning: None,
        })
    }
    async fn close_chapter(&self, _: ChapterInput) -> Result<ChapterResult> {
        anyhow::bail!("No chapter split is scripted")
    }
}

struct Server {
    base: String,
    client: Client,
    harness: Arc<Harness>,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<Result<()>>,
    directory: tempfile::TempDir,
    background: EpisodeBackground,
}
impl Server {
    async fn start(provider: Arc<dyn Provider>, tools: ToolRegistry) -> Self {
        Self::start_with_services(
            provider,
            tools,
            WebServices::default(),
            bubblepaw_core::ExecutionMode::Native,
        )
        .await
    }
    async fn start_with_services(
        provider: Arc<dyn Provider>,
        tools: ToolRegistry,
        services: WebServices,
        execution_mode: bubblepaw_core::ExecutionMode,
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let assets = directory.path().join("dist");
        std::fs::create_dir(&assets).unwrap();
        std::fs::write(assets.join("index.html"), "<html>test frontend</html>").unwrap();
        let store = bubblepaw_core::Store::open(directory.path().join("archive.sqlite")).unwrap();
        let summary = Arc::new(
            SummarizingContext::new(
                store.clone(),
                provider.clone(),
                SummaryConfig::default(),
                None,
            )
            .unwrap(),
        );
        // WebEpisodeModels only scripts episode-only summaries. docs/lcm-contract.md
        // "Gather": episode-only input is an explicit policy, never a silent
        // fallback from the default full-prefix fork.
        let worker = EpisodeWorker::new_with_config(
            store.clone(),
            Arc::new(WebEpisodeModels),
            summary.clone(),
            bubblepaw_runner::episodes::EpisodeConfig {
                summary_input: bubblepaw_runner::episodes::EpisodeSummaryInput::Episode,
                ..Default::default()
            },
        )
        .unwrap();
        summary.set_episode_worker(&worker);
        let template = ModelRequest {
            model: "fixture".into(),
            system_prompt: String::new(),
            messages: vec![],
            tools: tools.definitions(),
            max_output_tokens: None,
            ..Default::default()
        };
        let harness = Arc::new(Harness::new(
            store.clone(),
            provider,
            tools,
            HarnessConfig {
                execution_mode,
                model: "fixture".into(),
                context: summary.clone(),
                ..Default::default()
            },
        ));
        let background = EpisodeBackground::start(
            harness.subscribe(),
            store.clone(),
            summary,
            worker,
            Arc::new(move |_: &str| -> anyhow::Result<ModelRequest> { Ok(template.clone()) }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let config = WebConfig {
            workspace: directory.path().display().to_string(),
            model: "fixture".into(),
            provider: "demo".into(),
            read_only: true,
        };
        let (shutdown, receiver) = oneshot::channel();
        let host = harness.clone();
        let task = tokio::spawn(serve_with_services(
            listener,
            host,
            store,
            config,
            assets,
            services,
            async {
                let _ = receiver.await;
            },
        ));
        Self {
            base,
            client: Client::new(),
            harness,
            shutdown,
            task,
            directory,
            background,
        }
    }
    async fn post(&self, path: &str, input: Value) -> (StatusCode, Value) {
        let response = self
            .client
            .post(format!("{}{path}", self.base))
            .json(&input)
            .send()
            .await
            .unwrap();
        (response.status(), response.json().await.unwrap())
    }
    async fn get(&self, path: &str) -> Value {
        self.client
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    async fn create(&self) -> String {
        let (status, value) = self
            .post("/api/sessions", json!({"title":"Transport fixture"}))
            .await;
        assert_eq!(status, StatusCode::OK);
        value["id"].as_str().unwrap().into()
    }
    async fn stop(self) {
        self.background.shutdown(Duration::from_secs(3)).await;
        self.shutdown.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn actual_http_runs_native_tool_and_projects_history_without_archive_writes() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("fixture.txt"),
        "Native fixture contents",
    )
    .unwrap();
    let mut tools = ToolRegistry::new();
    let processes = register_native_tools(&mut tools, workspace.path(), true).unwrap();
    let artifacts =
        Arc::new(ArtifactStore::new(workspace.path(), workspace.path().join("artifacts")).unwrap());
    let repl = Arc::new(CodeMode::new(workspace.path().to_path_buf(), artifacts).unwrap());
    repl.register(&mut tools).unwrap();
    let provider = ScriptedProvider::with_steps(
        vec![ScriptedStep::Calls {
            calls: vec![ToolCall {
                id: "read-fixture".into(),
                name: "js".into(),
                arguments: json!({"code":"const r=await tools.read({path:'fixture.txt'}); text(r.content);"}),
            }],
        }],
        "The native file was read.",
    );
    let server = Server::start_with_services(
        Arc::new(provider),
        tools,
        WebServices::default(),
        bubblepaw_core::ExecutionMode::Js,
    )
    .await;
    let id = server.create().await;
    let (status, run) = server
        .post(
            &format!("/api/sessions/{id}/run"),
            json!({"text":"Read the fixture"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(run["status"], "running");
    assert!(run.get("text").is_none());
    server.harness.wait_for_idle(&id).await.unwrap();
    let before = server.harness.store().storage_stats().unwrap();
    let view = server.get(&format!("/api/sessions/{id}")).await;
    assert_eq!(view["session"]["status"], "completed");
    assert_eq!(view["session"]["message_count"], 4);
    assert_eq!(view["messages"][2]["role"], "tool");
    assert!(
        view["messages"][2]["content"]
            .as_str()
            .unwrap()
            .contains("Native fixture contents")
    );
    assert_eq!(view["messages"][3]["content"], "The native file was read.");
    assert!(view["runs"][0].get("text").is_none());
    assert!(view["active_run_id"].is_null());
    assert_eq!(view["live_paragraphs"], json!([]));
    for _ in 0..5 {
        server.get("/api/sessions").await;
        server.get(&format!("/api/sessions/{id}")).await;
    }
    let after = server.harness.store().storage_stats().unwrap();
    assert_eq!(before.transaction_count, after.transaction_count);
    assert_eq!(before.message_count, after.message_count);
    assert_eq!(before.wal_bytes, after.wal_bytes);
    let (status, fork) = server
        .post(
            &format!("/api/sessions/{id}/fork"),
            json!({"title":"Forked fixture"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let fork_view = server
        .get(&format!("/api/sessions/{}", fork["id"].as_str().unwrap()))
        .await;
    assert_eq!(fork_view["messages"].as_array().unwrap().len(), 4);
    assert_eq!(fork["parent_session_id"], id);
    let config = server.get("/api/config").await;
    assert_eq!(config["provider"], "demo");
    assert_eq!(config["session_limit"], 64);
    assert!(config.get("credentials").is_none());
    // No serving database was written alongside the canonical SQLite archive.
    let files: Vec<_> = std::fs::read_dir(server.directory.path())
        .unwrap()
        .map(|file| file.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !files
            .iter()
            .any(|file| file.contains("serving") || file.contains("projection"))
    );
    processes.shutdown().await;
    repl.shutdown().await;
    server.stop().await;
}

#[tokio::test]
async fn messages_before_pages_older_history_and_validates_cursors_and_limits() {
    let server = Server::start(
        Arc::new(ScriptedProvider::new("fixture")),
        ToolRegistry::new(),
    )
    .await;
    let id = server.create().await;
    let empty_id = server.create().await;
    let foreign_id = server.create().await;
    let seed = |session: &str, index: usize| Message {
        id: format!("{session}-message-{index}"),
        session_id: session.into(),
        role: if index.is_multiple_of(2) {
            Role::User
        } else {
            Role::Assistant
        },
        origin: None,
        content: format!("Historical message {index}"),
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: Some(json!({"private":"history-provider-state"})),
        is_error: false,
        created_at: "2026-10-13T00:00:00Z".into(),
    };
    let mut history: Vec<_> = (0..450).map(|index| seed(&id, index)).collect();
    let foreign = seed(&foreign_id, 0);
    server.harness.store().append_batch(&history).unwrap();
    server
        .harness
        .store()
        .append_batch(std::slice::from_ref(&foreign))
        .unwrap();
    // Compare display records while confirming canonical provider state survives.
    assert!(
        server.harness.store().messages(&id).unwrap()[0]
            .provider_state
            .is_some()
    );
    for message in &mut history {
        message.provider_state = None;
    }
    let expected = serde_json::to_value(&history).unwrap();
    let expected = expected.as_array().unwrap();
    // Missing-session lookup repairs the catalog; verify that error before
    // measuring pagination reads against already resolved session records.
    let missing = server
        .client
        .get(format!("{}/api/sessions/unknown/messages", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let missing_error: Value = missing.json().await.unwrap();
    assert_eq!(missing_error.as_object().unwrap().len(), 1);
    assert!(
        missing_error["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty())
    );
    // Drain seeded serving projections before measuring read-only requests.
    server.harness.store().flush_serving().unwrap();
    let before = server.harness.store().storage_stats().unwrap();
    let view = server.get(&format!("/api/sessions/{id}")).await;
    assert_eq!(view["session"]["message_count"], 450);
    assert_eq!(view["messages"], json!(&expected[250..]));
    assert_eq!(view["limits"]["messages_truncated"], true);
    let mut combined = view["messages"].as_array().unwrap().clone();
    for (first, end, has_more) in [(150, 250, true), (50, 150, true), (0, 50, false)] {
        let cursor = combined[0]["id"].as_str().unwrap();
        let page = server
            .get(&format!("/api/sessions/{id}/messages?before={cursor}"))
            .await;
        assert_eq!(page["messages"], json!(&expected[first..end]));
        assert_eq!(page["has_more"], has_more);
        assert!(
            page["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|message| message.get("provider_state").is_none())
        );
        let mut older = page["messages"].as_array().unwrap().clone();
        older.extend(combined);
        combined = older;
    }
    assert_eq!(&combined, expected);
    let at_start = server
        .get(&format!(
            "/api/sessions/{id}/messages?before={}",
            history[0].id
        ))
        .await;
    assert_eq!(at_start, json!({"messages":[], "has_more":false}));
    let exact_page = server
        .get(&format!(
            "/api/sessions/{id}/messages?before={}",
            history[100].id
        ))
        .await;
    assert_eq!(exact_page["messages"], json!(&expected[..100]));
    assert_eq!(exact_page["has_more"], false);
    for limit in [1, 200] {
        let page = server
            .get(&format!("/api/sessions/{id}/messages?limit={limit}"))
            .await;
        assert_eq!(page["messages"], json!(&expected[450 - limit..]));
        assert_eq!(page["has_more"], true);
    }
    let latest = server.get(&format!("/api/sessions/{id}/messages")).await;
    assert_eq!(latest["messages"], json!(&expected[350..]));
    assert_eq!(latest["has_more"], true);
    let empty = server
        .get(&format!("/api/sessions/{empty_id}/messages"))
        .await;
    assert_eq!(empty, json!({"messages":[], "has_more":false}));
    let mut errors = vec![
        (
            format!("/api/sessions/{id}/messages?before=unknown"),
            StatusCode::BAD_REQUEST,
        ),
        (
            format!("/api/sessions/{id}/messages?before={}", foreign.id),
            StatusCode::BAD_REQUEST,
        ),
    ];
    for limit in ["0", "201", "-1", "nope", "1.5", "", "18446744073709551616"] {
        errors.push((
            format!("/api/sessions/{id}/messages?limit={limit}"),
            StatusCode::BAD_REQUEST,
        ));
    }
    for (path, status) in errors {
        let response = server
            .client
            .get(format!("{}{path}", server.base))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{path}");
        let error: Value = response.json().await.unwrap();
        assert_eq!(error.as_object().unwrap().len(), 1);
        assert!(
            error["error"]
                .as_str()
                .is_some_and(|error| !error.is_empty())
        );
    }
    let after = server.harness.store().storage_stats().unwrap();
    assert_eq!(before.transaction_count, after.transaction_count);
    assert_eq!(before.message_count, after.message_count);
    assert_eq!(before.wal_bytes, after.wal_bytes);
    server.stop().await;
}

struct GatedProvider {
    entered: Semaphore,
    release: Semaphore,
    callback: Mutex<Option<TextCallback>>,
    requests: Mutex<Vec<ModelRequest>>,
}
impl GatedProvider {
    fn new() -> Self {
        Self {
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
            callback: Mutex::new(None),
            requests: Mutex::new(Vec::new()),
        }
    }
    async fn entered(&self) {
        self.entered.acquire().await.unwrap().forget();
    }
    fn emit(&self, text: &str) {
        self.callback.lock().unwrap().as_ref().unwrap()(text);
    }
}
#[async_trait]
impl Provider for GatedProvider {
    async fn generate(
        &self,
        request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        *self.callback.lock().unwrap() = Some(on_text.clone());
        on_text("A live paragraph.\n\n");
        self.entered.add_permits(1);
        tokio::select! { _ = cancellation.cancelled() => anyhow::bail!("cancelled"), permit = self.release.acquire() => permit.unwrap().forget() }
        Ok(ModelResponse {
            text: "A completed answer.".into(),
            provider_state: Some(json!({"continuation":"private-fixture-marker"})),
            ..Default::default()
        })
    }
}

async fn next_sse<B: AsRef<[u8]>>(
    stream: &mut (impl futures_util::Stream<Item = std::result::Result<B, reqwest::Error>> + Unpin),
    buffer: &mut String,
) -> String {
    loop {
        if let Some(end) = buffer.find("\n\n") {
            let frame = buffer[..end].to_owned();
            buffer.drain(..end + 2);
            return frame;
        }
        let bytes = tokio::time::timeout(Duration::from_secs(3), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        buffer.push_str(std::str::from_utf8(bytes.as_ref()).unwrap());
    }
}

#[tokio::test]
async fn paragraphs_reconnect_cancel_followup_and_resume_use_same_durable_harness() {
    let provider = Arc::new(GatedProvider::new());
    let server = Server::start(provider.clone(), ToolRegistry::new()).await;
    let id = server.create().await;
    let response = server
        .client
        .get(format!("{}/api/events", server.base))
        .send()
        .await
        .unwrap();
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    assert!(
        next_sse(&mut stream, &mut buffer)
            .await
            .contains("event: reset")
    );
    let (status, _) = server
        .post(
            &format!("/api/sessions/{id}/run"),
            json!({"text":"Initial draft"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    provider.entered().await;
    loop {
        if next_sse(&mut stream, &mut buffer)
            .await
            .contains("\"type\":\"paragraph\"")
        {
            break;
        }
    }
    let view = server.get(&format!("/api/sessions/{id}")).await;
    assert_eq!(view["messages"].as_array().unwrap().len(), 1);
    assert_eq!(view["live_paragraphs"], json!(["A live paragraph."]));
    let before = server.harness.store().storage_stats().unwrap();
    provider.emit("More transient text.\n\n");
    loop {
        if next_sse(&mut stream, &mut buffer)
            .await
            .contains("More transient text")
        {
            break;
        }
    }
    server.get(&format!("/api/sessions/{id}")).await;
    let after = server.harness.store().storage_stats().unwrap();
    assert_eq!(before.transaction_count, after.transaction_count);
    assert_eq!(before.message_count, after.message_count);
    assert_eq!(before.wal_bytes, after.wal_bytes);
    let reconnected = server
        .client
        .get(format!("{}/api/events", server.base))
        .header("Last-Event-ID", "0")
        .send()
        .await
        .unwrap();
    let mut reconnected_stream = reconnected.bytes_stream();
    // Resuming replays the log (docs/web-contract.md): run_started is event 1.
    let replayed = next_sse(&mut reconnected_stream, &mut String::new()).await;
    assert!(replayed.contains("id: 1"), "{replayed}");
    assert!(replayed.contains("\"type\":\"run_started\""), "{replayed}");
    let view = server.get(&format!("/api/sessions/{id}")).await;
    assert_eq!(view["live_paragraphs"].as_array().unwrap().len(), 2);
    let (status, _) = server
        .post(
            &format!("/api/sessions/{id}/run"),
            json!({"text":"Conflict"}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = server
        .post(&format!("/api/sessions/{id}/fork"), json!({}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        server
            .post(
                &format!("/api/sessions/{id}/steer"),
                json!({"text":"Steer at next boundary"})
            )
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        server
            .post(
                &format!("/api/sessions/{id}/followup"),
                json!({"text":"Next task"})
            )
            .await
            .0,
        StatusCode::OK
    );
    let (status, cancelled) = server
        .post(&format!("/api/sessions/{id}/cancel"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cancelled["requested"], true);
    server.harness.wait_for_idle(&id).await.unwrap();
    let cancelled_view = server.get(&format!("/api/sessions/{id}")).await;
    assert_eq!(cancelled_view["session"]["status"], "cancelled");
    assert_eq!(
        cancelled_view["pending_inputs"].as_array().unwrap().len(),
        2
    );
    assert_eq!(cancelled_view["live_paragraphs"], json!([]));
    let (status, _) = server
        .post(&format!("/api/sessions/{id}/resume"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK);
    provider.entered().await;
    provider.release.add_permits(3);
    server.harness.wait_for_idle(&id).await.unwrap();
    let view = server.get(&format!("/api/sessions/{id}")).await;
    assert_eq!(view["session"]["status"], "completed");
    assert_eq!(view["pending_inputs"], json!([]));
    assert!(
        !serde_json::to_string(&view)
            .unwrap()
            .contains("provider_state")
    );
    assert!(
        !serde_json::to_string(&view)
            .unwrap()
            .contains("private-fixture-marker")
    );
    assert!(
        server
            .harness
            .store()
            .messages(&id)
            .unwrap()
            .iter()
            .any(|message| message.provider_state.is_some())
    );
    {
        let requests = provider.requests.lock().unwrap();
        assert!(requests.iter().any(|request| {
            request
                .messages
                .iter()
                .any(|message| message.content == "Next task")
        }));
        assert!(requests.iter().any(|request| {
            request
                .messages
                .iter()
                .any(|message| message.content == "Steer at next boundary")
        }));
    }
    // Keep both SSE subscribers open while shutting down: shutdown must drain.
    server.stop().await;
}

#[tokio::test]
async fn provider_routes_share_the_hosts_origin_host_and_json_boundary() {
    let setup_directory = tempfile::tempdir().unwrap();
    let target = setup_directory.path().join("config.toml");
    let original = "# provider boundary fixture\n[providers.local]\nkind = 'scripted'\n";
    std::fs::write(&target, original).unwrap();
    let options = LoadOptions {
        workspace: setup_directory.path().join("workspace"),
        home: Some(setup_directory.path().join("home")),
        custom_file: Some(target.clone()),
        env: Vec::new(),
        cli: Vec::new(),
    };
    let launched = EffectiveConfig::load(options.clone()).unwrap();
    let routes = provider_router(ProviderSetup::new(options, &launched).unwrap());
    let server = Server::start_with_services(
        Arc::new(ScriptedProvider::new("fixture")),
        ToolRegistry::new(),
        WebServices {
            provider_routes: Some(routes),
            ..WebServices::default()
        },
        bubblepaw_core::ExecutionMode::Native,
    )
    .await;
    let listed = server.get("/api/providers").await;
    assert_eq!(listed["target"]["path"], target.to_string_lossy().as_ref());
    assert!(
        listed["providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|provider| provider["name"] == "local")
    );
    let session_origin = server
        .client
        .post(format!("{}/api/sessions", server.base))
        .header("Origin", "https://example.com")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(session_origin.status(), StatusCode::FORBIDDEN);
    let origin_error: Value = session_origin.json().await.unwrap();
    let session_host = server
        .client
        .post(format!("{}/api/sessions", server.base))
        .header("Host", "example.com:8788")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(session_host.status(), StatusCode::FORBIDDEN);
    let host_error: Value = session_host.json().await.unwrap();
    for (method, path) in [
        (reqwest::Method::PUT, "/api/providers/local"),
        (reqwest::Method::DELETE, "/api/providers/local"),
        (reqwest::Method::POST, "/api/providers/local/test"),
        (reqwest::Method::PUT, "/api/roles/main"),
    ] {
        let origin = server
            .client
            .request(method.clone(), format!("{}{path}", server.base))
            .header("Origin", "https://example.com")
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(origin.status(), StatusCode::FORBIDDEN);
        assert_eq!(origin.json::<Value>().await.unwrap(), origin_error);
        let host = server
            .client
            .request(method, format!("{}{path}", server.base))
            .header("Host", "example.com:8788")
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(host.status(), StatusCode::FORBIDDEN);
        assert_eq!(host.json::<Value>().await.unwrap(), host_error);
    }
    let missing_json = server
        .client
        .post(format!("{}/api/sessions", server.base))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(missing_json.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let post_error: Value = missing_json.json().await.unwrap();
    assert_eq!(post_error["error"], "POST requires application/json");
    for path in ["/api/providers/local", "/api/roles/main"] {
        let response = server
            .client
            .put(format!("{}{path}", server.base))
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(
            response.json::<Value>().await.unwrap(),
            json!({"error":"PUT requires application/json"})
        );
    }
    let missing_delete_json = server
        .client
        .delete(format!("{}/api/providers/local", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(
        missing_delete_json.status(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    assert_eq!(
        missing_delete_json.json::<Value>().await.unwrap(),
        json!({"error":"DELETE requires application/json"})
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), original);
    server.stop().await;
}

#[path = "web_transport/scope.rs"]
mod scope;

#[tokio::test]
async fn local_boundary_rejects_foreign_origins_hosts_and_invalid_json() {
    let server = Server::start(
        Arc::new(ScriptedProvider::new("fixture")),
        ToolRegistry::new(),
    )
    .await;
    let id = server.create().await;
    let response = server
        .client
        .post(format!("{}/api/sessions", server.base))
        .header("Origin", "https://example.com")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(response.json::<Value>().await.unwrap()["error"].is_string());
    let response = server
        .client
        .get(format!("{}/api/config", server.base))
        .header("Host", "example.com:8788")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = server
        .client
        .get(format!("{}/api/config", server.base))
        .header("Origin", format!("{}1", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = server
        .client
        .post(format!("{}/api/sessions", server.base))
        .header("Origin", &server.base)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = server
        .client
        .post(format!("{}/api/sessions", server.base))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert!(response.json::<Value>().await.unwrap()["error"].is_string());
    let response = server
        .client
        .post(format!("{}/api/sessions", server.base))
        .header("content-type", "application/json")
        .body("{")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(response.json::<Value>().await.unwrap()["error"].is_string());
    assert_eq!(
        server
            .post(&format!("/api/sessions/{id}/run"), json!({"text":"  "}))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let (status, value) = server.post("/api/sessions/unknown/cancel", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(value["error"].is_string());
    let response = server
        .client
        .get(format!("{}/api/nonexistent", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(response.json::<Value>().await.unwrap()["error"].is_string());
    let response = server
        .client
        .post(format!("{}/api/sessions", server.base))
        .header("content-type", "application/json")
        .body(format!("{{\"title\":\"{}\"}}", "a".repeat(300 * 1024)))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(response.json::<Value>().await.unwrap()["error"].is_string());
    server.stop().await;
}

struct UncertainTool {
    entered: Semaphore,
    count: AtomicUsize,
}
#[async_trait]
impl Tool for UncertainTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            approval: Default::default(),
            name: "uncertain_effect".into(),
            description: "Fixture effect that never returns".into(),
            input_schema: json!({"type":"object"}),
            read_only: false,
        }
    }
    async fn invoke(&self, _context: ToolContext, _input: Value) -> Result<ToolResult> {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.entered.add_permits(1);
        std::future::pending().await
    }
}

#[tokio::test]
async fn unknown_effect_error_remains_visible_and_resume_does_not_replay_it() {
    let tool = Arc::new(UncertainTool {
        entered: Semaphore::new(0),
        count: AtomicUsize::new(0),
    });
    let call = ToolCall {
        id: "same-effect".into(),
        name: "uncertain_effect".into(),
        arguments: json!({}),
    };
    let provider = ScriptedProvider::with_steps(
        vec![
            ScriptedStep::Calls {
                calls: vec![call.clone()],
            },
            ScriptedStep::Calls { calls: vec![call] },
        ],
        "fixture",
    );
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone()).unwrap();
    let server = Server::start(Arc::new(provider), tools).await;
    let id = server.create().await;
    assert_eq!(
        server
            .post(
                &format!("/api/sessions/{id}/run"),
                json!({"text":"Attempt fixture"})
            )
            .await
            .0,
        StatusCode::OK
    );
    tokio::time::timeout(Duration::from_secs(5), tool.entered.acquire())
        .await
        .expect("native uncertain_effect dispatch did not enter")
        .unwrap()
        .forget();
    server
        .post(&format!("/api/sessions/{id}/cancel"), json!({}))
        .await;
    server.harness.wait_for_idle(&id).await.unwrap();
    let view = server.get(&format!("/api/sessions/{id}")).await;
    assert!(view["messages"].as_array().unwrap().iter().any(|message| {
        message["is_error"] == true
            && message["content"]
                .as_str()
                .unwrap()
                .contains("outcome unknown")
    }));
    assert_eq!(
        server
            .post(&format!("/api/sessions/{id}/resume"), json!({}))
            .await
            .0,
        StatusCode::OK
    );
    server.harness.wait_for_idle(&id).await.unwrap();
    let view = server.get(&format!("/api/sessions/{id}")).await;
    assert_eq!(view["session"]["status"], "failed");
    assert!(
        view["runs"].as_array().unwrap().last().unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("reused")
    );
    assert_eq!(tool.count.load(Ordering::Relaxed), 1);
    server.stop().await;
}

#[tokio::test]
async fn serving_view_can_be_rebuilt_from_canonical_history() {
    let server = Server::start(
        Arc::new(ScriptedProvider::new("Saved answer")),
        ToolRegistry::new(),
    )
    .await;
    let id = server.create().await;
    server
        .harness
        .prompt(&id, "Saved question", RunOptions::default())
        .await
        .unwrap();
    let original = server.get(&format!("/api/sessions/{id}")).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (stop, receiver) = oneshot::channel();
    let task = tokio::spawn(serve_with_services(
        listener,
        server.harness.clone(),
        server.harness.store().clone(),
        WebConfig {
            workspace: "fixture".into(),
            model: "fixture".into(),
            provider: "demo".into(),
            read_only: true,
        },
        server.directory.path().join("dist"),
        WebServices::default(),
        async {
            let _ = receiver.await;
        },
    ));
    let before = server
        .harness
        .store()
        .storage_stats()
        .unwrap()
        .transaction_count;
    let rebuilt: Value = server
        .client
        .get(format!("{base}/api/sessions/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(original["messages"], rebuilt["messages"]);
    assert_eq!(original["runs"], rebuilt["runs"]);
    assert_eq!(
        before,
        server
            .harness
            .store()
            .storage_stats()
            .unwrap()
            .transaction_count
    );
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
    server.stop().await;
}

#[tokio::test]
async fn nonloopback_bind_and_missing_built_assets_fail_explicitly() {
    let directory = tempfile::tempdir().unwrap();
    let store = bubblepaw_core::Store::in_memory().unwrap();
    let harness = Arc::new(Harness::new(
        store.clone(),
        Arc::new(ScriptedProvider::new("fixture")),
        ToolRegistry::new(),
        HarnessConfig::default(),
    ));
    let config = WebConfig {
        workspace: "fixture".into(),
        model: "fixture".into(),
        provider: "demo".into(),
        read_only: true,
    };
    let serve = |listener| {
        serve_with_services(
            listener,
            harness.clone(),
            store.clone(),
            config.clone(),
            directory.path().into(),
            WebServices::default(),
            async {},
        )
    };
    let listener = TcpListener::bind("0.0.0.0:0").await.unwrap();
    let error = serve(listener).await.unwrap_err();
    assert!(error.to_string().contains("loopback"));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let error = serve(listener).await.unwrap_err();
    assert!(error.to_string().contains("Build the web frontend"));
}

struct LargeWebModel;
#[async_trait]
impl Provider for LargeWebModel {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(ModelResponse {
            text: " abc".repeat(70_000),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn web_commits_wake_the_enabled_episode_worker() {
    let server = Server::start(Arc::new(LargeWebModel), ToolRegistry::new()).await;
    let id = server.create().await;
    for text in ["first web task", "continue the web task"] {
        let (status, _) = server
            .post(&format!("/api/sessions/{id}/run"), json!({"text":text}))
            .await;
        assert_eq!(status, StatusCode::OK);
        server.harness.wait_for_idle(&id).await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if !server
                .harness
                .store()
                .context_episodes(&id)
                .unwrap()
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("Committed web events must wake the always-on worker");
    assert!(
        server
            .harness
            .store()
            .context_chapters(&id)
            .unwrap()
            .is_empty()
    );
    let view = server.get(&format!("/api/sessions/{id}")).await;
    assert_eq!(view["session"]["status"], "completed");
    assert_eq!(view["session"]["message_count"], 4);
    server.stop().await;
}

fn frame_id(frame: &str) -> Option<u64> {
    frame
        .lines()
        .find_map(|line| line.strip_prefix("id: "))
        .and_then(|id| id.trim().parse().ok())
}
fn frame_data(frame: &str) -> Value {
    frame
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .unwrap_or(Value::Null)
}

#[path = "web_transport/sse_resume.rs"]
mod sse_resume;
