//! Scripted live-config acceptance: registry changes append notices with a frozen chat prefix.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ExecutionMode, Harness, HarnessConfig, MessageOrigin, ModelRequest,
    ModelResponse, Observation, ObservationMode, ObservationRecord, ObservationSink, Provider,
    Role, RunOptions, Store, TextCallback, ToolCall, ToolRegistry,
};
use bubblepaw_mcp::McpHost;
use bubblepaw_runner::{
    agent_handles::register_agent_handles,
    agents::{AgentRegistry, AgentTools},
    artifacts::ArtifactStore,
    codemode::CodeMode,
    config::{EffectiveConfig, LoadOptions},
    live_config::LiveConfig,
    profiles::child_agent_host_live,
    provider::LiveProviderConfig,
    register_native_tools_with_exec,
    tools::ProcessTools,
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
struct Script;
#[async_trait]
impl Provider for Script {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(ModelResponse {
            text: "fixture done".into(),
            ..Default::default()
        })
    }
}

#[derive(Default)]
struct Sink(Mutex<Vec<ObservationRecord>>);
impl ObservationSink for Sink {
    fn record(&self, record: ObservationRecord) {
        self.0.lock().unwrap().push(record);
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    file: PathBuf,
    live: Arc<LiveConfig>,
    mcp: Arc<McpHost>,
    processes: Arc<ProcessTools>,
    agents: Arc<AgentTools>,
    harness: Arc<Harness>,
    sink: Arc<Sink>,
    session: String,
    store: Store,
}
fn config(extra: &str) -> String {
    format!(
        "[model]\nmain='demo/first'\n[approvals]\nconfirm='allow'\n[roles]\nsummary='main'\nchapter='main'\ndecider='main'\napprover='main'\n{extra}"
    )
}
impl Fixture {
    async fn new(text: &str) -> Self {
        Self::with_provider(text, ExecutionMode::Js, Arc::new(Script)).await
    }
    async fn with_provider(text: &str, mode: ExecutionMode, script: Arc<dyn Provider>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("config.toml");
        std::fs::write(&file, text).unwrap();
        let effective = EffectiveConfig::load(LoadOptions {
            workspace: root.path().to_owned(),
            home: None,
            custom_file: Some(file.clone()),
            env: vec![],
            cli: vec![],
        })
        .unwrap();
        let settings = effective.settings().unwrap();
        let store = Store::open(root.path().join("archive.db")).unwrap();
        let session = store.create_session("fixture").unwrap().id;
        let mut tools = ToolRegistry::new();
        let processes = register_native_tools_with_exec(
            &mut tools,
            root.path(),
            false,
            settings.exec.clone(),
            root.path().join("state"),
        )
        .unwrap();
        let mcp = Arc::new(McpHost::start(&settings.mcp, root.path()).await);
        let providers = Arc::new(LiveProviderConfig::new(effective.clone()).unwrap());
        let live = LiveConfig::new(
            effective,
            providers.clone(),
            mcp.clone(),
            processes.clone(),
            store.clone(),
        )
        .unwrap();
        let repl = Arc::new(
            CodeMode::new(
                root.path().to_owned(),
                Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap()),
            )
            .unwrap(),
        );
        let sink = Arc::new(Sink::default());
        let mut harness_config = HarnessConfig {
            model: "demo/first".into(),
            execution_mode: mode,
            system_prompt: "Fixed native live-reload acceptance instructions.".into(),
            observation: Some(Observation {
                sink: sink.clone(),
                mode: ObservationMode::Metadata,
                max_payload_bytes: 0,
            }),
            ..Default::default()
        };
        let live_providers = providers.clone();
        let scripted_provider = script.clone();
        harness_config.provider_resolver = Some(Arc::new(move || {
            (live_providers.main_model(), scripted_provider.clone())
        }));
        live.configure_harness(&mut harness_config);
        let agents = child_agent_host_live(
            store.clone(),
            script.clone(),
            &tools,
            &repl,
            &harness_config,
            root.path(),
            true,
            None,
            settings.agents,
            Arc::new(AgentRegistry::default()),
            processes.clone(),
            providers.clone(),
        )
        .unwrap();
        agents.register(&mut tools).unwrap();
        register_agent_handles(&mut tools, agents.clone()).unwrap();
        repl.register(&mut tools).unwrap();
        live.bind(tools, &agents).unwrap();
        let harness = Arc::new(Harness::new(
            store.clone(),
            script.clone(),
            live.registry(),
            harness_config,
        ));
        Self {
            _root: root,
            file,
            live,
            mcp,
            processes,
            agents,
            harness,
            sink,
            session,
            store,
        }
    }
    async fn turn(&self, prompt: &str) -> String {
        let run = self
            .harness
            .prompt(&self.session, prompt, RunOptions::default())
            .await
            .unwrap();
        assert_eq!(run.status, "completed", "{:?}", run.error);
        run.id
    }
    /// The metrics of the first request a run built, which carries its declaration.
    fn first_build(&self, run: &str) -> Value {
        self.sink
            .0
            .lock()
            .unwrap()
            .iter()
            .find(|record| record.kind == "context_built" && record.scope.run_id == run)
            .expect("run builds a request")
            .metrics
            .clone()
    }
    async fn shutdown(&self) {
        self.agents.shutdown().await.unwrap();
        self.processes.shutdown().await;
        self.mcp.shutdown(Duration::from_secs(2)).await;
    }
}

struct NativeReloadScript {
    file: Mutex<PathBuf>,
    added_config: String,
    requests: Mutex<Vec<ModelRequest>>,
}

#[async_trait]
impl Provider for NativeReloadScript {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let step = {
            let mut requests = self.requests.lock().unwrap();
            let step = requests.len();
            requests.push(request);
            step
        };
        let (name, arguments) = match step {
            0 => {
                // Stage the edit inside generation: the turn-start config check
                // has already run and the first request has been captured.
                std::fs::write(&*self.file.lock().unwrap(), &self.added_config)?;
                ("reload", json!({}))
            }
            1 => ("tool_search", json!({"query":"select:mcp__late__echo"})),
            2 => (
                "call_tool",
                json!({"name":"mcp__late__echo","arguments":{"text":"native late MCP transport proof"}}),
            ),
            3 => {
                std::fs::write(&*self.file.lock().unwrap(), config(""))?;
                ("reload", json!({}))
            }
            4 => ("tool_search", json!({"query":"select:mcp__late__echo"})),
            5 => (
                "call_tool",
                json!({"name":"mcp__late__echo","arguments":{"text":"must not run after removal"}}),
            ),
            6 => {
                return Ok(ModelResponse {
                    text: "native reload complete".into(),
                    ..Default::default()
                });
            }
            _ => anyhow::bail!("unexpected native reload request {step}"),
        };
        Ok(ModelResponse {
            tool_calls: vec![ToolCall {
                id: format!("native-reload-{step}"),
                name: name.into(),
                arguments,
            }],
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn native_reload_connects_and_removes_mcp_mid_run_with_frozen_declarations() {
    let command = toml::Value::String(env!("CARGO_BIN_EXE_bubblepaw-runner-fake-mcp").into());
    let script = Arc::new(NativeReloadScript {
        file: Mutex::new(PathBuf::new()),
        added_config: config(&format!(
            "[mcp.servers.late]\ncommand={command}\napproval='none'\ninclude_tools=['echo']\n"
        )),
        requests: Mutex::default(),
    });
    let fixture = Fixture::with_provider(&config(""), ExecutionMode::Native, script.clone()).await;
    *script.file.lock().unwrap() = fixture.file.clone();
    fixture
        .turn("connect, discover, invoke and remove the late MCP server")
        .await;
    {
        let requests = script.requests.lock().unwrap();
        assert_eq!(requests.len(), 7);
        let frozen_tools = serde_json::to_vec(&requests[0].tools).unwrap();
        let frozen_prompt = requests[0].system_prompt.as_bytes();
        // This is the full live host catalog, including child orchestration.
        for tool in &requests[0].tools {
            assert!(
                !tool.description.contains("tools."),
                "{}: {}",
                tool.name,
                tool.description
            );
            assert!(
                !tool
                    .description
                    .split(|c: char| !c.is_alphanumeric())
                    .any(|word| word.eq_ignore_ascii_case("js")),
                "{}: {}",
                tool.name,
                tool.description
            );
        }
        assert!(requests[0].tools.iter().any(|tool| tool.name == "reload"));
        assert!(
            requests[0]
                .tools
                .iter()
                .any(|tool| tool.name == "tool_search")
        );
        assert!(
            requests[0]
                .tools
                .iter()
                .any(|tool| tool.name == "call_tool")
        );
        for request in requests.iter() {
            assert_eq!(serde_json::to_vec(&request.tools).unwrap(), frozen_tools);
            assert_eq!(request.system_prompt.as_bytes(), frozen_prompt);
            assert!(
                !request
                    .tools
                    .iter()
                    .any(|tool| tool.name == "mcp__late__echo" || tool.name == "js")
            );
            assert!(!request.system_prompt.contains("mcp__late__echo"));
        }
        let (added_notice, delta) = requests[1]
            .messages
            .iter()
            .find_map(|message| inventory_delta(message).map(|delta| (message, delta)))
            .expect("reload appends a structured inventory notice");
        assert_eq!(
            delta["added"],
            json!([{"name":"mcp__late__echo","server":"late","deferred":true}])
        );
        assert!(added_notice.content.contains("call_tool"));
        assert!(!added_notice.content.contains("input_schema"));
        assert!(!added_notice.content.contains("inputSchema"));
        assert!(!added_notice.content.contains("fake echo"));
        let discovered = requests[2]
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Tool)
            .unwrap();
        assert!(!discovered.is_error, "{}", discovered.content);
        let discovered: Value = serde_json::from_str(&discovered.content).unwrap();
        assert_eq!(discovered["tools"][0]["name"], "mcp__late__echo");
        assert_eq!(discovered["tools"][0]["inputSchema"]["type"], "object");
        assert_eq!(discovered["tools"][0]["server"], "late");
        assert!(
            !added_notice
                .content
                .contains(&serde_json::to_string(&discovered["tools"][0]["inputSchema"]).unwrap())
        );
        assert!(
            requests[4]
                .messages
                .iter()
                .filter_map(inventory_delta)
                .any(|delta| {
                    delta["removed"]
                        .as_array()
                        .unwrap()
                        .contains(&json!("mcp__late__echo"))
                })
        );
        let missing = requests[5]
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Tool)
            .unwrap();
        let missing: Value = serde_json::from_str(&missing.content).unwrap();
        assert!(missing["tools"].as_array().unwrap().is_empty());
        assert_eq!(missing["unknown"], json!(["mcp__late__echo"]));
    }
    let messages = fixture.store.messages(&fixture.session).unwrap();
    assert!(messages.iter().any(|message| message.role == Role::Tool
        && !message.is_error
        && message.content == "native late MCP transport proof"));
    assert!(messages.iter().any(|message| message.role == Role::Tool
        && message.is_error
        && message.content.contains("mcp__late__echo")
        && message.content.contains("unavailable")));
    assert!(fixture.mcp.tools().is_empty());
    assert!(
        fixture
            .live
            .registry()
            .get_host("mcp__late__echo")
            .is_none()
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn republished_tools_report_inventory_change_once_without_declaring_a_prefix_change() {
    let fixture = Fixture::new(&config("")).await;
    let first = fixture.turn("warm").await;
    let built = fixture.first_build(&first);
    assert_eq!(built["classification"]["kind"], "first");
    assert_eq!(
        built["declared"], false,
        "the first observation never declares"
    );
    // A late MCP server changes the inventory while this chat keeps its declarations.
    let command = toml::Value::String(env!("CARGO_BIN_EXE_bubblepaw-runner-fake-mcp").into());
    std::fs::write(
        &fixture.file,
        config(&format!(
            "[mcp.servers.late]\ncommand={command}\napproval='none'\n"
        )),
    )
    .unwrap();
    let report = fixture.live.reload(None).await;
    assert!(
        report.applied && !report.changed.is_empty(),
        "{:?}",
        report.errors
    );
    let republished = fixture.turn("after republish").await;
    let built = fixture.first_build(&republished);
    assert_eq!(built["classification"]["kind"], "append");
    assert_eq!(built["declared"], false);
    assert!(built["declaration_reason"].is_null());
    {
        let records = fixture.sink.0.lock().unwrap();
        let changes = records
            .iter()
            .filter(|record| record.kind == "context_change" && record.scope.run_id == republished)
            .collect::<Vec<_>>();
        assert_eq!(changes.len(), 1);
        assert_eq!(
            changes[0].metrics,
            json!({"reason":"tools_reloaded","prefix_changed":false,"generation":fixture.live.registry().generation()})
        );
    }
    // A later run with the same inventory reports no new change.
    let unchanged = fixture.turn("unchanged").await;
    let built = fixture.first_build(&unchanged);
    assert_eq!(built["declared"], false);
    assert!(built["declaration_reason"].is_null());
    assert!(
        !fixture
            .sink
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|record| record.kind == "context_change" && record.scope.run_id == unchanged)
    );
    fixture.shutdown().await;
}

fn inventory_delta(message: &bubblepaw_core::Message) -> Option<Value> {
    if message.origin != Some(MessageOrigin::Generated)
        || !message
            .content
            .contains("<harness kind=\"tools_reloaded\">")
    {
        return None;
    }
    serde_json::from_str(message.content.lines().find(|line| line.starts_with('{'))?).ok()
}
