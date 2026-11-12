//! Scripted acceptance for frozen native declarations, deferred anchors and live inventory.
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ContextPreparation, ExecutionMode, Harness, HarnessConfig, ModelRequest,
    ModelResponse, PrepareCall, PreparedContext, Provider, Role, RunOptions, Store, TextCallback,
    Tool, ToolCall, ToolContext, ToolDefinition, ToolDispatcher, ToolRegistry, ToolResult,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex, RwLock, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

struct Script {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
}
#[async_trait]
impl Provider for Script {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow!("Unexpected request"))
    }
}
struct Echo(&'static str);
#[async_trait]
impl Tool for Echo {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.0.into(),
            description: format!("Inspect {} records", self.0),
            input_schema: match self.0 {
                "new_core" => json!({
                    "type":"object", "additionalProperties":false,
                    "properties":{
                        "value":{"type":"string"},
                        "options":{"type":"object","properties":{
                            "kinds":{"type":"array","items":{"enum":["exact","fuzzy"]}},
                            "limit":{"type":"integer","minimum":1,"maximum":100}
                        },"required":["kinds"],"additionalProperties":false}
                    }, "required":["value"]
                }),
                "mcp__late__lookup" => json!({
                    "type":"object", "properties":{
                        "value":{"type":"string"},
                        "deferred_only_detail":{"type":"string","description":"DEFERRED_SCHEMA_MUST_NOT_APPEAR_IN_NOTICE"}
                    }, "additionalProperties":false
                }),
                _ => {
                    json!({"type":"object","properties":{"value":{"type":"string"}},"additionalProperties":false})
                }
            },
            read_only: true,
            approval: Default::default(),
        }
    }
    async fn invoke(&self, _: ToolContext, input: Value) -> Result<ToolResult> {
        Ok(ToolResult::text(input.to_string()))
    }
}

struct Publish {
    registry: Weak<RwLock<ToolRegistry>>,
    signal: Arc<AtomicBool>,
    request_refresh: bool,
}
#[async_trait]
impl Tool for Publish {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "reload".into(),
            description: "Publish updated tools".into(),
            input_schema: json!({"type":"object"}),
            read_only: true,
            approval: Default::default(),
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        unreachable!()
    }
    async fn invoke_with_dispatcher(
        &self,
        _: ToolContext,
        _: Value,
        dispatcher: ToolDispatcher,
    ) -> Result<ToolResult> {
        let inventory = self.registry.upgrade().unwrap();
        let mut tools = inventory.write().unwrap();
        publish_inventory(&mut tools)?;
        self.signal.store(true, Ordering::SeqCst);
        if self.request_refresh {
            dispatcher.refresh_tools_next_round();
        }
        Ok(ToolResult::text("published"))
    }
}

fn publish_inventory(tools: &mut ToolRegistry) -> Result<()> {
    tools.remove("mcp__old__lookup", "server disconnected");
    tools.remove("former_core", "removed by reload");
    tools.register_host(Arc::new(Echo("mcp__late__lookup")))?;
    tools.set_deferred("mcp__late__lookup", true);
    tools.set_tool_server("mcp__late__lookup", "late.server");
    tools.register_host(Arc::new(Echo("new_core")))?;
    *tools = tools.clone().with_generation(2);
    Ok(())
}

struct ApplyOnce {
    pending: Arc<AtomicBool>,
    applied: AtomicBool,
    prepares: AtomicUsize,
}
#[async_trait]
impl ContextPreparation for ApplyOnce {
    async fn prepare(&self, call: PrepareCall<'_>) -> Result<PreparedContext> {
        let mut request = call.request;
        let prepares = self.prepares.fetch_add(1, Ordering::SeqCst) + 1;
        if self.applied.load(Ordering::SeqCst)
            || (prepares >= 3 && self.pending.load(Ordering::SeqCst))
        {
            // Simulate an applied view folding prior notices into its memory.
            request
                .messages
                .retain(|message| !message.content.contains("Tool inventory changed"));
        }
        Ok(PreparedContext {
            request,
            notice: None,
            usage: None,
        })
    }
    fn cold_message_indexes(&self, _: &str, request: &ModelRequest) -> Result<Vec<usize>> {
        Ok((!request.messages.is_empty())
            .then_some(0)
            .into_iter()
            .collect())
    }
    fn take_prefix_change(&self, _: &str) -> Option<String> {
        (self.prepares.load(Ordering::SeqCst) >= 3 && self.pending.swap(false, Ordering::SeqCst))
            .then(|| {
                self.applied.store(true, Ordering::SeqCst);
                "lcm_apply".into()
            })
    }
}

fn call(name: &str, arguments: Value) -> ModelResponse {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    ModelResponse {
        tool_calls: vec![ToolCall {
            id: format!("call_{}", CALLS.fetch_add(1, Ordering::Relaxed)),
            name: name.into(),
            arguments,
        }],
        ..Default::default()
    }
}
fn done() -> ModelResponse {
    ModelResponse {
        text: "done".into(),
        ..Default::default()
    }
}

/// A scripted access wrapper: core tests exercise dispatch and dynamic help;
/// the runner's JS tests exercise JavaScript parsing and execution.
struct JsDispatch;
#[async_trait]
impl Tool for JsDispatch {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "js".into(),
            description: "Scripted JS adapter".into(),
            input_schema: json!({"type":"object","properties":{"code":{"type":"string"}},"required":["code"]}),
            read_only: true,
            approval: Default::default(),
        }
    }
    fn definition_in(&self, registry: &ToolRegistry) -> ToolDefinition {
        ToolDefinition {
            description: serde_json::to_string(&registry.capabilities()).unwrap(),
            ..self.definition()
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        unreachable!()
    }
    async fn invoke_with_dispatcher(
        &self,
        context: ToolContext,
        input: Value,
        dispatcher: ToolDispatcher,
    ) -> Result<ToolResult> {
        let target: Value = serde_json::from_str(input["code"].as_str().unwrap())?;
        dispatcher
            .invoke(
                context,
                target["name"].as_str().unwrap(),
                target["arguments"].clone(),
            )
            .await
    }
}
fn routed(mode: ExecutionMode, name: &str, arguments: Value) -> ModelResponse {
    match mode {
        ExecutionMode::Native => call("call_tool", json!({"name":name,"arguments":arguments})),
        ExecutionMode::Js => call(
            "js",
            json!({"code":json!({"name":name,"arguments":arguments}).to_string()}),
        ),
    }
}
fn frozen(request: &ModelRequest) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(
        &json!({"system":request.system_prompt,"tools":request.tools}),
    )?)
}

struct Fixture {
    harness: Harness,
    provider: Arc<Script>,
    store: Store,
    session: String,
    inventory: Arc<RwLock<ToolRegistry>>,
}
fn fixture(
    mode: ExecutionMode,
    deferred: bool,
    responses: Vec<ModelResponse>,
    apply: bool,
    request_refresh: bool,
) -> Result<Fixture> {
    fixture_in_store(
        mode,
        deferred,
        responses,
        apply,
        request_refresh,
        Store::in_memory()?,
    )
}
fn fixture_in_store(
    mode: ExecutionMode,
    deferred: bool,
    responses: Vec<ModelResponse>,
    apply: bool,
    request_refresh: bool,
    store: Store,
) -> Result<Fixture> {
    let inventory = Arc::new(RwLock::new(ToolRegistry::new()));
    let signal = Arc::new(AtomicBool::new(false));
    {
        let mut tools = inventory.write().unwrap();
        tools.register_host(Arc::new(Echo("former_core")))?;
        tools.register_host(Arc::new(Echo("mcp__old__lookup")))?;
        tools.set_deferred("mcp__old__lookup", deferred);
        tools.set_tool_server("mcp__old__lookup", "original.server");
        tools.register_host(Arc::new(Publish {
            registry: Arc::downgrade(&inventory),
            signal: signal.clone(),
            request_refresh,
        }))?;
        tools.register(Arc::new(JsDispatch))?;
        *tools = tools.clone().with_generation(1);
    }
    let provider = Arc::new(Script {
        responses: Mutex::new(responses.into()),
        requests: Mutex::default(),
    });
    let session = store.create_session("native snapshot")?.id;
    let resolver = inventory.clone();
    let mut config = HarnessConfig {
        execution_mode: mode,
        system_prompt: "Fixed instructions".into(),
        registry_resolver: Some(Arc::new(move || resolver.read().unwrap().clone())),
        ..Default::default()
    };
    if apply {
        config.context = Arc::new(ApplyOnce {
            pending: signal,
            applied: AtomicBool::new(false),
            prepares: AtomicUsize::new(0),
        });
    }
    let harness = Harness::new(store.clone(), provider.clone(), ToolRegistry::new(), config);
    Ok(Fixture {
        harness,
        provider,
        store,
        session,
        inventory,
    })
}

#[tokio::test]
async fn native_deferred_anchors_and_declarations_are_stable_across_turns() -> Result<()> {
    let f = fixture(
        ExecutionMode::Native,
        true,
        vec![done(), done()],
        false,
        false,
    )?;
    let template = f.harness.request_template(&f.session)?;
    assert!(
        template
            .system_prompt
            .contains("original.server: 1 tools (mcp__old__lookup)")
    );
    assert!(
        template
            .system_prompt
            .contains("tool_search(\"original.server\")")
    );
    assert!(!template.system_prompt.contains("input_schema"));
    for _ in 0..2 {
        assert_eq!(
            f.harness
                .prompt(&f.session, "turn", RunOptions::default())
                .await?
                .status,
            "completed"
        );
    }
    let requests = f.provider.requests.lock().unwrap();
    assert_eq!(requests[0].system_prompt, template.system_prompt);
    assert_eq!(requests[0].system_prompt, requests[1].system_prompt);
    assert_eq!(
        serde_json::to_vec(&requests[0].tools)?,
        serde_json::to_vec(&requests[1].tools)?
    );
    let names: Vec<_> = requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert!(
        names.contains(&"former_core")
            && names.contains(&"tool_search")
            && names.contains(&"call_tool")
    );
    assert!(!names.contains(&"mcp__old__lookup"));
    Ok(())
}

#[tokio::test]
async fn anchors_are_absent_for_core_mcp_js_or_an_empty_effective_view() -> Result<()> {
    for (mode, deferred, allowlist) in [
        (ExecutionMode::Native, false, None),
        (ExecutionMode::Js, true, None),
        (
            ExecutionMode::Native,
            true,
            Some(vec!["former_core".into()]),
        ),
    ] {
        let f = fixture(mode, deferred, vec![done()], false, false)?;
        assert_eq!(
            f.harness
                .prompt(
                    &f.session,
                    "turn",
                    RunOptions {
                        tool_names: allowlist,
                        ..Default::default()
                    }
                )
                .await?
                .status,
            "completed"
        );
        let requests = f.provider.requests.lock().unwrap();
        assert!(!requests[0].system_prompt.contains("Deferred MCP tools"));
        if mode == ExecutionMode::Native && !deferred {
            assert!(
                requests[0]
                    .tools
                    .iter()
                    .any(|tool| tool.name == "mcp__old__lookup")
            );
        }
    }
    Ok(())
}

#[path = "native_tool_snapshot/inventory.rs"]
mod inventory;
#[path = "native_tool_snapshot/lifetime.rs"]
mod lifetime;
