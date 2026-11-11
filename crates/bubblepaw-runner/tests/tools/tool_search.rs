//! The `js` description (always-shown signatures, deferred names) and tool
//! discovery inside js with `tools.$search` / `tools.$list`.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, Role,
    RunOptions, Store, TextCallback, Tool, ToolCall, ToolContext, ToolDefinition, ToolRegistry,
    ToolResult,
};
use bubblepaw_runner::{artifacts::ArtifactStore, codemode::CodeMode};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

struct Schemed(&'static str, &'static str, Value);
#[async_trait]
impl Tool for Schemed {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            approval: Default::default(),
            name: self.0.into(),
            description: self.1.into(),
            read_only: true,
            input_schema: self.2.clone(),
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        Ok(ToolResult::text(json!({"ran":self.0}).to_string()))
    }
}

/// The user message is the js code; records each request's js description.
#[derive(Default)]
struct Cells(Mutex<Vec<String>>);
#[async_trait]
impl Provider for Cells {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        assert_eq!(request.tools.len(), 1);
        self.0
            .lock()
            .unwrap()
            .push(request.tools[0].description.clone());
        let last = request.messages.last().unwrap();
        if last.role == Role::User {
            return Ok(ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "js".into(),
                    arguments: json!({"code":last.content}),
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

struct Fixture {
    _root: tempfile::TempDir,
    store: Store,
    harness: Harness,
    provider: Arc<Cells>,
    session: String,
}
fn registry(mode: &Arc<CodeMode>) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.show_always(["alpha", "not_registered"]);
    tools
        .register_host(Arc::new(Schemed(
            "alpha",
            "Alpha tool.",
            json!({"type":"object","properties":{"a":{"type":"string"},"b":{"type":"integer"}},"required":["a"]}),
        )))
        .unwrap();
    tools
        .register_host(Arc::new(Schemed(
            "beta_search",
            "Search notes by keyword.",
            json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}),
        )))
        .unwrap();
    tools
        .register_host(Arc::new(Schemed(
            "gamma",
            "Report gamma rays. Also measures alpha particles.",
            json!({"type":"object"}),
        )))
        .unwrap();
    mode.register(&mut tools).unwrap();
    // Registered after js: must still appear in its description.
    tools
        .register_host(Arc::new(Schemed(
            "late",
            "Registered late.",
            json!({"type":"object"}),
        )))
        .unwrap();
    tools
}
impl Fixture {
    fn new(restrict: impl FnOnce(ToolRegistry) -> ToolRegistry) -> Self {
        let root = tempfile::tempdir().unwrap();
        let artifacts =
            Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap());
        let mode = Arc::new(CodeMode::new(root.path().to_owned(), artifacts).unwrap());
        let tools = restrict(registry(&mode));
        let store = Store::in_memory().unwrap();
        let session = store.create_session("search").unwrap().id;
        let provider = Arc::new(Cells::default());
        let harness = Harness::new(
            store.clone(),
            provider.clone(),
            tools,
            HarnessConfig {
                execution_mode: bubblepaw_core::ExecutionMode::Js,
                ..Default::default()
            },
        );
        Self {
            _root: root,
            store,
            harness,
            provider,
            session,
        }
    }
    async fn cell(&self, code: &str) -> Value {
        self.harness
            .prompt(&self.session, code, RunOptions::default())
            .await
            .unwrap();
        let messages = self.store.messages(&self.session).unwrap();
        let last = messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Tool)
            .unwrap();
        assert!(!last.is_error, "{}", last.content);
        serde_json::from_str(last.content.split("\n[---]\n").next().unwrap()).unwrap()
    }
}

#[tokio::test]
async fn description_shows_signatures_and_deferred_names_deterministically() {
    let fixture = Fixture::new(|tools| tools);
    fixture.cell("return 1").await;
    fixture.cell("return 2").await;
    let descriptions = fixture.provider.0.lock().unwrap().clone();
    assert_eq!(descriptions.len(), 4);
    assert!(descriptions.iter().all(|d| *d == descriptions[0]));
    let description = &descriptions[0];
    assert!(
        description.contains("tools.alpha({a, b?}) -> JSON: Alpha tool.\n"),
        "{description}"
    );
    assert!(
        description.contains(
            "Other tools (load the schema with tools.$search(\"select:name\")): beta_search, environments, gamma, late\n"
        ),
        "{description}"
    );
    for hidden in [
        "tools.gamma(",
        "tools.beta_search(",
        "tools.environments(",
        "not_registered",
    ] {
        assert!(!description.contains(hidden), "{hidden}: {description}");
    }
}

#[tokio::test]
async fn search_select_keyword_and_limit() {
    let fixture = Fixture::new(|tools| tools);
    let selected = fixture
        .cell("return await tools.$search('select:gamma, nope,alpha')")
        .await;
    let names: Vec<_> = selected["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["gamma", "alpha"]);
    assert_eq!(selected["unknown"], json!(["nope"]));
    assert_eq!(
        selected["tools"][1]["inputSchema"]["required"],
        json!(["a"])
    );
    assert_eq!(selected["tools"][1]["description"], "Alpha tool.");
    let keyword = fixture
        .cell("return await tools.$search('SEARCH notes')")
        .await;
    assert_eq!(keyword["tools"][0]["name"], "beta_search");
    // Name matches outrank description matches.
    let ranked = fixture.cell("return await tools.$search('alpha')").await;
    let names: Vec<_> = ranked["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["alpha", "gamma"]);
    let limited = fixture
        .cell("return await tools.$search('alpha', {limit: 1})")
        .await;
    assert_eq!(limited["tools"].as_array().unwrap().len(), 1);
    let none = fixture.cell("return await tools.$search('zzz')").await;
    assert_eq!(none["tools"], json!([]));
    // Searching leaves the js description unchanged.
    let descriptions = fixture.provider.0.lock().unwrap().clone();
    assert!(descriptions.iter().all(|d| *d == descriptions[0]));
}

#[tokio::test]
async fn list_and_deferred_call_without_search() {
    let fixture = Fixture::new(|tools| tools);
    let listed = fixture.cell("return await tools.$list()").await;
    let entries = listed.as_array().unwrap();
    let names: Vec<_> = entries
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["alpha", "beta_search", "environments", "gamma", "late"]
    );
    let gamma = entries.iter().find(|e| e["name"] == "gamma").unwrap();
    assert_eq!(gamma["summary"], "Report gamma rays.");
    assert_eq!(gamma["shownAlways"], false);
    let alpha = entries.iter().find(|e| e["name"] == "alpha").unwrap();
    assert_eq!(alpha["shownAlways"], true);
    let ran = fixture.cell("return await tools.late({})").await;
    assert_eq!(ran["ran"], "late");
}

#[tokio::test]
async fn restricted_tools_are_not_listed_or_found() {
    let fixture = Fixture::new(|tools| {
        tools.restricted_capabilities(&["alpha".into(), "environments".into()])
    });
    let listed = fixture.cell("return await tools.$list()").await;
    let names: Vec<_> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["alpha", "environments"]);
    let selected = fixture
        .cell("return await tools.$search('select:gamma,alpha')")
        .await;
    assert_eq!(selected["unknown"], json!(["gamma"]));
    let keyword = fixture
        .cell("return await tools.$search('gamma rays')")
        .await;
    assert_eq!(keyword["tools"], json!([]));
    let description = fixture.provider.0.lock().unwrap()[0].clone();
    assert!(!description.contains("gamma"), "{description}");
    assert!(
        description.contains("tools.alpha({a, b?})"),
        "{description}"
    );
}
