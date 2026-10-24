use super::*;
use serde_json::json;
use std::future::{Future, poll_fn};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::task::Poll;
use std::time::Duration;
use tokio::sync::Notify;

#[derive(Default)]
struct RecordingProvider {
    code_surface: bool,
    requests: Mutex<Vec<ModelRequest>>,
}
#[async_trait]
impl Provider for RecordingProvider {
    fn capabilities(&self, _: &str) -> Capabilities {
        Capabilities {
            tool_surface: if self.code_surface {
                ToolSurface::Code
            } else {
                ToolSurface::Direct
            },
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
        Ok(ModelResponse {
            text: "done".into(),
            ..Default::default()
        })
    }
}

struct Capability {
    name: &'static str,
    approval: Approval,
    calls: Arc<AtomicUsize>,
    pending: bool,
    read_only: bool,
    blocked: Option<Arc<Notify>>,
}
impl Capability {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            approval: Approval::None,
            calls: Arc::default(),
            pending: false,
            read_only: true,
            blocked: None,
        }
    }
}
#[async_trait]
impl Tool for Capability {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.into(),
            description: "fixture capability".into(),
            input_schema: json!({"type":"object","properties":{"value":{"type":"string"}},"required":["value"],"additionalProperties":false}),
            approval: self.approval,
            read_only: self.read_only,
        }
    }
    async fn invoke(&self, context: ToolContext, input: Value) -> Result<ToolResult> {
        assert!(!context.invocation_id.is_empty());
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(entered) = &self.blocked {
            entered.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(ToolResult {
            content: input["value"].as_str().unwrap().into(),
            is_error: false,
            pending: self.pending,
        })
    }
}

struct JsBridge;
#[async_trait]
impl Tool for JsBridge {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "js".into(),
            description: "composition".into(),
            input_schema: json!({"type":"object"}),
            approval: Approval::None,
            read_only: true,
        }
    }
    fn definition_in(&self, registry: &ToolRegistry) -> ToolDefinition {
        ToolDefinition {
            description: registry
                .capabilities()
                .iter()
                .map(|tool| tool.name.clone())
                .collect::<Vec<_>>()
                .join(","),
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
        dispatcher.invoke(context, "read", input).await
    }
}

fn fixture(
    config: HarnessConfig,
    capability: Arc<Capability>,
) -> Result<(Store, Session, Harness, Arc<RecordingProvider>)> {
    let store = Store::in_memory()?;
    let session = store.create_session("single tool")?;
    let provider = Arc::new(RecordingProvider::default());
    let mut tools = ToolRegistry::new();
    tools.register_host(capability)?;
    tools.register(Arc::new(JsBridge))?;
    let harness = Harness::new(store.clone(), provider.clone(), tools, config);
    Ok((store, session, harness, provider))
}
fn native_config() -> HarnessConfig {
    HarnessConfig {
        execution_mode: ExecutionMode::Native,
        ..Default::default()
    }
}
fn names(definitions: Vec<ToolDefinition>) -> Vec<String> {
    definitions
        .into_iter()
        .map(|definition| definition.name)
        .collect()
}

mod dispatch_contract;
mod single_tool;
mod surface;
