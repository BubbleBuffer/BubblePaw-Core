//! Native-default declarations and dispatch using the runner's real tool pack.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    ApprovalMode, ApprovalPolicy, CancellationToken, ExecutionMode, Harness, HarnessConfig,
    ModelRequest, ModelResponse, Provider, Role, RunOptions, Store, TextCallback, ToolCall,
    ToolRegistry,
};
use bubblepaw_runner::{
    artifacts::ArtifactStore,
    codemode::CodeMode,
    profiles::workspace_profiles,
    tools::{ProcessTools, register_native_tools},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

struct Probe {
    mode: ExecutionMode,
    requests: Mutex<Vec<ModelRequest>>,
}

#[async_trait]
impl Provider for Probe {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let has = |name: &str| request.tools.iter().any(|tool| tool.name == name);
        assert_eq!(has("read"), self.mode == ExecutionMode::Native);
        assert_eq!(has("js"), self.mode == ExecutionMode::Js);
        let first = request
            .messages
            .last()
            .is_some_and(|m| m.role == Role::User);
        self.requests.lock().unwrap().push(request);
        if !first {
            return Ok(ModelResponse {
                text: "finished".into(),
                ..Default::default()
            });
        }
        let mut tool_calls = Vec::new();
        if self.mode == ExecutionMode::Native {
            tool_calls.push(ToolCall {
                id: "native-read".into(),
                name: "read".into(),
                arguments: json!({"path":"sample.txt"}),
            });
        }
        if self.mode == ExecutionMode::Js {
            tool_calls.push(ToolCall {
                id: "js-read".into(),
                name: "js".into(),
                arguments: json!({"code":"return await tools.read({path:'sample.txt'});"}),
            });
        }
        Ok(ModelResponse {
            tool_calls,
            ..Default::default()
        })
    }
}

struct Fixture {
    root: tempfile::TempDir,
    store: Store,
    base_tools: ToolRegistry,
    repl: Arc<CodeMode>,
    processes: Arc<ProcessTools>,
    provider: Arc<Probe>,
    config: HarnessConfig,
    harness: Harness,
    session: String,
}

impl Fixture {
    fn new(mode: ExecutionMode) -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("sample.txt"), "native and js\n").unwrap();
        let store = Store::in_memory().unwrap();
        let session = store.create_session("execution-mode").unwrap().id;
        let mut tools = ToolRegistry::new();
        let processes = register_native_tools(&mut tools, root.path(), false).unwrap();
        // Child profiles install their own CodeMode over the host capability pack,
        // matching the production caller before root CodeMode registration.
        let base_tools = tools.clone();
        let repl = Arc::new(
            CodeMode::new(
                root.path().to_owned(),
                Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap()),
            )
            .unwrap(),
        );
        repl.register(&mut tools).unwrap();
        let provider = Arc::new(Probe {
            mode,
            requests: Mutex::new(Vec::new()),
        });
        let config = HarnessConfig {
            execution_mode: mode,
            ..Default::default()
        };
        let harness = Harness::new(store.clone(), provider.clone(), tools, config.clone());
        Self {
            root,
            store,
            base_tools,
            repl,
            processes,
            provider,
            config,
            harness,
            session,
        }
    }

    async fn shutdown(&self) {
        self.repl.shutdown().await;
        self.processes.shutdown().await;
    }
}

#[tokio::test]
async fn native_builtin_catalog_is_core_stable_and_describes_direct_calls() {
    let f = Fixture::new(ExecutionMode::Native);
    let definitions = f
        .harness
        .effective_tool_definitions(&f.session, &RunOptions::default())
        .unwrap();
    let repeated = f
        .harness
        .effective_tool_definitions(&f.session, &RunOptions::default())
        .unwrap();
    assert_eq!(
        serde_json::to_value(&definitions).unwrap(),
        serde_json::to_value(repeated).unwrap()
    );
    let names: Vec<_> = definitions.iter().map(|tool| tool.name.as_str()).collect();
    assert!(!names.contains(&"js"));
    for builtin in f.base_tools.capabilities() {
        assert!(
            !f.base_tools.is_deferred(&builtin.name),
            "{} must be core",
            builtin.name
        );
        assert!(
            names.contains(&builtin.name.as_str()),
            "{} must be declared",
            builtin.name
        );
    }
    for definition in &definitions {
        assert!(
            !definition.description.contains("tools."),
            "{}: {}",
            definition.name,
            definition.description
        );
        assert!(
            !definition
                .description
                .split(|c: char| !c.is_alphanumeric())
                .any(|word| word.eq_ignore_ascii_case("js")),
            "{}: {}",
            definition.name,
            definition.description
        );
    }
    f.shutdown().await;
}

#[tokio::test]
async fn native_model_calls_existing_workspace_tool_without_js() {
    let f = Fixture::new(ExecutionMode::Native);
    let result = f
        .harness
        .prompt(&f.session, "read it", RunOptions::default())
        .await
        .unwrap();
    assert_eq!(result.status, "completed", "{:?}", result.error);
    let messages = f.store.messages(&f.session).unwrap();
    assert_eq!(messages.iter().filter(|m| m.role == Role::Tool).count(), 1);
    let call = &messages
        .iter()
        .find(|m| m.role == Role::Assistant)
        .unwrap()
        .tool_calls[0];
    assert_eq!(call.name, "read");
    let tool = messages.iter().find(|m| m.role == Role::Tool).unwrap();
    assert!(!tool.is_error, "{}", tool.content);
    assert_eq!(
        serde_json::from_str::<Value>(&tool.content).unwrap()["content"],
        "native and js\n"
    );
    assert!(f.store.pending_effects(&f.session).unwrap().is_empty());
    f.shutdown().await;
}

#[tokio::test]
async fn native_never_executes_the_js_adapter() {
    let f = Fixture::new(ExecutionMode::Native);
    let result = f
        .harness
        .call_tool(
            &f.session,
            "js",
            json!({"code":"return 1;"}),
            RunOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(result.is_error, "{}", result.content);
    assert!(
        result.content.contains("Unknown tool"),
        "{}",
        result.content
    );
    assert!(f.provider.requests.lock().unwrap().is_empty());
    assert!(f.store.pending_effects(&f.session).unwrap().is_empty());
    f.shutdown().await;
}

#[tokio::test]
async fn effective_definitions_match_modes_and_keep_dynamic_js_host_help() {
    for mode in [ExecutionMode::Js, ExecutionMode::Native] {
        let f = Fixture::new(mode);
        let definitions = f
            .harness
            .effective_tool_definitions(&f.session, &RunOptions::default())
            .unwrap();
        let has = |name: &str| definitions.iter().any(|tool| tool.name == name);
        // Providers reject function tools whose parameters are not an object schema.
        for tool in &definitions {
            assert_eq!(tool.input_schema["type"], "object", "{} schema", tool.name);
        }
        assert_eq!(has("read"), mode == ExecutionMode::Native);
        assert_eq!(has("js"), mode == ExecutionMode::Js);
        if mode == ExecutionMode::Js {
            assert_eq!(definitions.len(), 1);
        }
        if let Some(js) = definitions.iter().find(|tool| tool.name == "js") {
            assert!(js.description.contains("tools.read("));
        }
        let selected = f
            .harness
            .effective_tool_definitions(
                &f.session,
                &RunOptions {
                    tool_names: Some(vec!["js".into(), "read".into()]),
                    ..Default::default()
                },
            )
            .unwrap();
        // tool_names restricts calls, not declarations: the profile's tools stay declared.
        let names = |tools: &[bubblepaw_core::ToolDefinition]| {
            tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&selected), names(&definitions));
        if let Some(js) = selected.iter().find(|tool| tool.name == "js") {
            // Composition and its help expose only allowed target capabilities.
            assert!(!js.description.contains("tools.list("));
            assert!(js.description.contains("tools.read("));
        }
        assert!(f.provider.requests.lock().unwrap().is_empty());
        f.shutdown().await;
    }
}

#[tokio::test]
async fn workspace_children_inherit_native_mode_and_read_only_capabilities() {
    let f = Fixture::new(ExecutionMode::Native);
    let profiles = workspace_profiles(
        f.store.clone(),
        f.provider.clone(),
        &f.base_tools,
        &f.repl,
        &f.config,
        f.root.path(),
        true,
    )
    .unwrap();
    assert_eq!(profiles.len(), 2);
    for profile in profiles {
        let session = f.store.create_session(&profile.name).unwrap().id;
        let definitions = profile
            .harness
            .effective_tool_definitions(&session, &RunOptions::default())
            .unwrap();
        assert!(!definitions.iter().any(|tool| tool.name == "js"));
        assert!(definitions.iter().any(|tool| tool.name == "read"));
        assert_eq!(
            definitions.iter().any(|tool| tool.name == "write"),
            profile.name == "workspace"
        );
        let run = profile
            .harness
            .prompt(&session, "read inherited tools", RunOptions::default())
            .await
            .unwrap();
        assert_eq!(run.status, "completed", "{:?}", run.error);
    }
    {
        let requests = f.provider.requests.lock().unwrap();
        assert!(
            requests
                .iter()
                .all(|request| !request.system_prompt.contains("JS"))
        );
    }
    f.shutdown().await;
}

#[tokio::test]
async fn model_free_js_call_matches_existing_script_return_without_provider_calls() {
    let f = Fixture::new(ExecutionMode::Js);
    let code = "return await tools.read({path:'sample.txt'});";
    let tool = f
        .harness
        .call_tool(
            &f.session,
            "js",
            json!({"code":code}),
            RunOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!tool.is_error, "{}", tool.content);
    let script_session = f.store.create_session("script-equivalent").unwrap().id;
    let script = f
        .harness
        .prompt(
            &script_session,
            "read via script",
            RunOptions {
                script: Some(code.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(script.status, "completed", "{:?}", script.error);
    assert_eq!(script.text.as_deref(), Some(tool.content.as_str()));
    assert!(f.provider.requests.lock().unwrap().is_empty());
    assert!(f.store.pending_effects(&f.session).unwrap().is_empty());
    assert!(f.store.pending_effects(&script_session).unwrap().is_empty());
    f.shutdown().await;
}

#[tokio::test]
async fn model_free_js_refuses_nested_interactive_and_auto_approval() {
    let f = Fixture::new(ExecutionMode::Js);
    for mode in [ApprovalMode::Ask, ApprovalMode::Auto] {
        let mut approval = ApprovalPolicy::default();
        approval.tools.insert("write".into(), mode);
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), f.harness.call_tool(
            &f.session, "js", json!({"code":"return await tools.write({path:'blocked.txt',content:'must not write'});"}),
            RunOptions { approval: Some(approval), ..Default::default() }, CancellationToken::new(),
        )).await.expect("model-free nested approval waited for a person or model").unwrap();
        assert!(result.is_error, "{}", result.content);
        assert!(
            result
                .content
                .contains("Interactive approval is unavailable"),
            "{}",
            result.content
        );
        assert!(!f.root.path().join("blocked.txt").exists());
        assert!(f.harness.pending_approvals(&f.session).unwrap().is_empty());
        assert!(f.store.pending_effects(&f.session).unwrap().is_empty());
    }
    assert!(f.provider.requests.lock().unwrap().is_empty());
    f.shutdown().await;
}

#[tokio::test]
async fn model_free_native_write_uses_workspace_checkpoints_and_effect_journal() {
    let f = Fixture::new(ExecutionMode::Native);
    let result = f
        .harness
        .call_tool(
            &f.session,
            "write",
            json!({"path":"written.txt","content":"checkpointed\n"}),
            RunOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        std::fs::read_to_string(f.root.path().join("written.txt")).unwrap(),
        "checkpointed\n"
    );
    assert_eq!(
        f.store
            .checkpoint_records_page(&f.session, None, 100)
            .unwrap()
            .len(),
        1
    );
    assert!(f.store.pending_effects(&f.session).unwrap().is_empty());
    assert!(f.provider.requests.lock().unwrap().is_empty());
    f.shutdown().await;
}
