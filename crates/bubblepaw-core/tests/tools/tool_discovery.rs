//! Native discovery and named calls exercise the same registered MCP-shaped
//! capability snapshot without requiring a JavaScript adapter or a provider.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ExecutionMode, Harness, HarnessConfig, ModelRequest, ModelResponse,
    Provider, RunOptions, Store, TextCallback, Tool, ToolContext, ToolDefinition, ToolRegistry,
    ToolResult,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const MCP_TOOL: &str = "mcp__calendar__events";
const MCP_SERVER: &str = "Family calendar / Europe (original server identity)";

struct NoProvider;
#[async_trait]
impl Provider for NoProvider {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        panic!("model-free tool calls must not invoke the provider")
    }
}

struct Calendar(Arc<AtomicUsize>);
#[async_trait]
impl Tool for Calendar {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: MCP_TOOL.into(),
            description: "Look up upcoming appointments.".into(),
            input_schema: json!({"type":"object","properties":{"day":{"type":"string"}},"required":["day"],"additionalProperties":false}),
            read_only: true,
            approval: Default::default(),
        }
    }
    async fn invoke(&self, _: ToolContext, input: Value) -> Result<ToolResult> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(ToolResult::text(
            json!({"day":input["day"],"events":["Lunch"]}).to_string(),
        ))
    }
}

struct Fixture {
    harness: Harness,
    store: Store,
    session: String,
    calls: Arc<AtomicUsize>,
}
impl Fixture {
    fn new() -> Self {
        let mut registry = ToolRegistry::new();
        let calls = Arc::new(AtomicUsize::new(0));
        registry
            .register_host(Arc::new(Calendar(calls.clone())))
            .unwrap();
        registry.mark_deferred(MCP_TOOL);
        registry.set_tool_server(MCP_TOOL, MCP_SERVER);
        let store = Store::in_memory().unwrap();
        let session = store.create_session("native discovery").unwrap().id;
        let harness = Harness::new(
            store.clone(),
            Arc::new(NoProvider),
            registry,
            HarnessConfig {
                execution_mode: ExecutionMode::Native,
                ..Default::default()
            },
        );
        Self {
            harness,
            store,
            session,
            calls,
        }
    }
    async fn call(&self, name: &str, arguments: Value, allowed: Option<Vec<String>>) -> ToolResult {
        self.harness
            .call_tool(
                &self.session,
                name,
                arguments,
                RunOptions {
                    tool_names: allowed,
                    ..Default::default()
                },
                CancellationToken::new(),
            )
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn native_search_discovers_deferred_mcp_by_name_description_and_original_server() {
    let fixture = Fixture::new();
    let definitions = fixture
        .harness
        .effective_tool_definitions(&fixture.session, &RunOptions::default())
        .unwrap();
    assert!(definitions.iter().any(|tool| tool.name == "tool_search"));
    assert!(definitions.iter().any(|tool| tool.name == "call_tool"));
    assert!(!definitions.iter().any(|tool| tool.name == MCP_TOOL));
    assert!(!definitions.iter().any(|tool| tool.name == "js"));
    for query in [MCP_TOOL, "appointments", "Europe"] {
        let result = fixture
            .call("tool_search", json!({"query":query}), None)
            .await;
        assert!(!result.is_error, "{}", result.content);
        let result: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(result["tools"][0]["name"], MCP_TOOL);
        assert_eq!(result["tools"][0]["server"], MCP_SERVER);
        assert_eq!(
            result["tools"][0]["inputSchema"]["required"],
            json!(["day"])
        );
    }
    assert_eq!(fixture.calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn native_named_call_invokes_deferred_mcp_and_keeps_argument_validation() {
    let fixture = Fixture::new();
    let result = fixture
        .call(
            "call_tool",
            json!({"name":MCP_TOOL,"arguments":{"day":"Monday"}}),
            None,
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        serde_json::from_str::<Value>(&result.content).unwrap()["day"],
        "Monday"
    );
    assert_eq!(fixture.calls.load(Ordering::Relaxed), 1);
    let invalid = fixture
        .call(
            "call_tool",
            json!({"name":MCP_TOOL,"arguments":{"day":42}}),
            None,
        )
        .await;
    assert!(invalid.is_error, "{}", invalid.content);
    assert!(invalid.content.contains("day"), "{}", invalid.content);
    assert_eq!(fixture.calls.load(Ordering::Relaxed), 1);
    assert!(
        fixture
            .store
            .pending_effects(&fixture.session)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn native_discovery_wrappers_stay_available_and_refuse_hidden_targets() {
    let fixture = Fixture::new();
    let hidden = fixture
        .call("tool_search", json!({"query":"appointments"}), Some(vec![]))
        .await;
    assert!(!hidden.is_error, "{}", hidden.content);
    assert_eq!(
        serde_json::from_str::<Value>(&hidden.content).unwrap()["tools"],
        json!([])
    );
    let denied = fixture
        .call(
            "call_tool",
            json!({"name":MCP_TOOL,"arguments":{"day":"Monday"}}),
            Some(vec![]),
        )
        .await;
    assert!(denied.is_error, "{}", denied.content);
    assert!(
        denied.content.contains("not available in this run"),
        "{}",
        denied.content
    );
    assert_eq!(fixture.calls.load(Ordering::Relaxed), 0);
    let allowed = fixture
        .call(
            "tool_search",
            json!({"query":"Europe"}),
            Some(vec![MCP_TOOL.into()]),
        )
        .await;
    assert!(!allowed.is_error, "{}", allowed.content);
    assert_eq!(
        serde_json::from_str::<Value>(&allowed.content).unwrap()["tools"][0]["name"],
        MCP_TOOL
    );
}

#[tokio::test]
async fn fork_delivery_targets_keep_the_normal_unknown_tool_error() {
    let fixture = Fixture::new();
    for target in ["submit_summary", "submit_chapter"] {
        let result = fixture
            .call(
                "call_tool",
                json!({"name":target,"arguments":{"summary":"branch memory"}}),
                None,
            )
            .await;
        assert!(result.is_error, "{}", result.content);
        assert!(
            result.content.contains(&format!("Unknown tool: {target}")),
            "{}",
            result.content
        );
    }
    assert_eq!(fixture.calls.load(Ordering::Relaxed), 0);
}
