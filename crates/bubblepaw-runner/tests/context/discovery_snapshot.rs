//! The optional JavaScript adapter exposes the run's effective capability
//! snapshot in its description, discovery and actual callable properties.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    Approval, ApprovalMode, ApprovalPolicy, CancellationToken, ExecutionMode, Harness,
    HarnessConfig, ModelRequest, ModelResponse, Provider, RunOptions, Store, TextCallback, Tool,
    ToolContext, ToolDefinition, ToolRegistry, ToolResult,
};
use bubblepaw_runner::{artifacts::ArtifactStore, codemode::CodeMode, environments, tools};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct NoProvider;
#[async_trait]
impl Provider for NoProvider {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        panic!("model-free JavaScript must not invoke a provider")
    }
}

struct Fixture {
    root: tempfile::TempDir,
    harness: Harness,
    mode: Arc<CodeMode>,
    processes: Arc<tools::ProcessTools>,
    session: String,
}
impl Fixture {
    fn new(read_only: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("sample.txt"), "readable").unwrap();
        let mut registry = ToolRegistry::new();
        let processes =
            tools::register_native_tools(&mut registry, root.path(), read_only).unwrap();
        let mode = Arc::new(
            CodeMode::new(
                root.path().to_owned(),
                Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap()),
            )
            .unwrap(),
        );
        mode.register(&mut registry).unwrap();
        let store = Store::in_memory().unwrap();
        let session = store.create_session("discovery snapshot").unwrap().id;
        let harness = Harness::new(
            store,
            Arc::new(NoProvider),
            registry,
            HarnessConfig {
                execution_mode: ExecutionMode::Js,
                ..Default::default()
            },
        );
        Self {
            root,
            harness,
            mode,
            processes,
            session,
        }
    }
    async fn js(&self, code: &str, allowed: Option<Vec<String>>) -> ToolResult {
        self.harness
            .call_tool(
                &self.session,
                "js",
                json!({"code":code}),
                RunOptions {
                    tool_names: allowed,
                    ..Default::default()
                },
                CancellationToken::new(),
            )
            .await
            .unwrap()
    }
    async fn shutdown(&self) {
        self.mode.shutdown().await;
        self.processes.shutdown().await;
    }
}

#[tokio::test]
async fn js_allowlist_filters_help_discovery_and_exec_invocation() {
    let fixture = Fixture::new(false);
    let options = RunOptions {
        tool_names: Some(vec!["read".into()]),
        ..Default::default()
    };
    let definitions = fixture
        .harness
        .effective_tool_definitions(&fixture.session, &options)
        .unwrap();
    let help = &definitions
        .iter()
        .find(|tool| tool.name == "js")
        .unwrap()
        .description;
    assert!(help.contains("tools.read("), "{help}");
    for hidden in [
        "tools.exec",
        "tools.edit",
        "tools.write",
        "tools.list",
        "tools.environments",
        "wait({handle",
    ] {
        assert!(!help.contains(hidden), "hidden {hidden}: {help}");
    }
    let discovered = fixture.js("return {list:await tools.$list(), search:await tools.$search('select:read,exec'), exposed:Object.keys(tools)};", options.tool_names.clone()).await;
    assert!(!discovered.is_error, "{}", discovered.content);
    let discovered: Value = serde_json::from_str(&discovered.content).unwrap();
    assert_eq!(
        discovered["list"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["read"]
    );
    assert_eq!(discovered["search"]["tools"][0]["name"], "read");
    assert_eq!(discovered["search"]["unknown"], json!(["exec"]));
    assert_eq!(discovered["exposed"], json!(["read", "$search", "$list"]));
    let refused = fixture
        .js(
            "return await tools.exec({command:'echo forbidden > unauthorized.txt'});",
            options.tool_names,
        )
        .await;
    assert!(refused.is_error, "{}", refused.content);
    assert!(
        refused.content.contains("not available in this run"),
        "{}",
        refused.content
    );
    assert!(!fixture.root.path().join("unauthorized.txt").exists());
    // The allowed target remains executable through the same adapter.
    let read = fixture
        .js(
            "return await tools.read({path:'sample.txt'});",
            Some(vec!["read".into()]),
        )
        .await;
    assert!(!read.is_error, "{}", read.content);
    assert_eq!(
        serde_json::from_str::<Value>(read.content.split("\n[---]\n").next().unwrap()).unwrap()["content"],
        "readable"
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn read_only_js_discovery_and_help_exclude_write_capabilities() {
    let fixture = Fixture::new(true);
    let definitions = fixture
        .harness
        .effective_tool_definitions(&fixture.session, &RunOptions::default())
        .unwrap();
    let help = &definitions
        .iter()
        .find(|tool| tool.name == "js")
        .unwrap()
        .description;
    for hidden in ["tools.exec", "tools.edit", "tools.write"] {
        assert!(!help.contains(hidden), "hidden {hidden}: {help}");
    }
    let result = fixture.js("return {list:await tools.$list(), search:await tools.$search('select:read,edit,write,exec')};", None).await;
    assert!(!result.is_error, "{}", result.content);
    let result: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(
        result["search"]["unknown"],
        json!(["edit", "write", "exec"])
    );
    assert_eq!(result["search"]["tools"][0]["name"], "read");
    let names = result["list"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(names.contains(&"read"));
    assert!(
        !names
            .iter()
            .any(|name| ["exec", "edit", "write"].contains(name))
    );
    fixture.shutdown().await;
}

#[test]
fn host_environment_controls_register_without_js_and_use_native_description() {
    let root = tempfile::tempdir().unwrap();
    let mode = Arc::new(
        CodeMode::new(
            root.path().to_owned(),
            Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap()),
        )
        .unwrap(),
    );
    let mut registry = ToolRegistry::new();
    environments::register(&mode, &mut registry).unwrap();
    assert!(registry.get("js").is_none());
    let definition = registry.get_host("environments").unwrap().definition();
    assert!(!definition.description.contains("JS"));
    assert!(!definition.description.contains("tools."));
}

struct Exec(Arc<AtomicUsize>);
#[async_trait]
impl Tool for Exec {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "exec".into(),
            description: "Execute a fixture command.".into(),
            input_schema: json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"],"additionalProperties":false}),
            read_only: false,
            approval: Approval::Confirm,
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(ToolResult::text("executed"))
    }
}

#[tokio::test]
async fn actual_js_native_and_named_exec_share_allowlist_and_approval_refusals() {
    let root = tempfile::tempdir().unwrap();
    let mode = Arc::new(
        CodeMode::new(
            root.path().to_owned(),
            Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap()),
        )
        .unwrap(),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry
        .register_host(Arc::new(Exec(calls.clone())))
        .unwrap();
    mode.register_js(&mut registry).unwrap();
    let store = Store::in_memory().unwrap();
    let session = store.create_session("actual JS refusal parity").unwrap().id;
    let harness = Harness::new(
        store.clone(),
        Arc::new(NoProvider),
        registry.clone(),
        HarnessConfig {
            execution_mode: ExecutionMode::Native,
            ..Default::default()
        },
    );
    let js_session = store
        .create_session("explicit JS refusal parity")
        .unwrap()
        .id;
    let js_harness = Harness::new(
        store.clone(),
        Arc::new(NoProvider),
        registry,
        HarnessConfig {
            execution_mode: ExecutionMode::Js,
            ..Default::default()
        },
    );
    for options in [
        RunOptions {
            tool_names: Some(vec!["read".into()]),
            ..Default::default()
        },
        RunOptions {
            approval: Some(ApprovalPolicy {
                confirm: ApprovalMode::Deny,
                ..Default::default()
            }),
            ..Default::default()
        },
    ] {
        let direct = harness
            .call_tool(
                &session,
                "exec",
                json!({"command":"fixture"}),
                options.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let named = harness
            .call_tool(
                &session,
                "call_tool",
                json!({"name":"exec","arguments":{"command":"fixture"}}),
                options.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(direct.is_error, "{}", direct.content);
        assert!(named.is_error, "{}", named.content);
        assert_eq!(direct.content, named.content);
        let js = js_harness.call_tool(&js_session, "js", json!({"code":"try { await tools.exec({command:'fixture'}); throw Error('executed'); } catch(error) { return error.message; }"}), options, CancellationToken::new()).await.unwrap();
        assert!(!js.is_error, "{}", js.content);
        assert_eq!(
            serde_json::from_str::<String>(&js.content).unwrap(),
            direct.content
        );
    }
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert!(store.pending_effects(&session).unwrap().is_empty());
    mode.shutdown().await;
}
