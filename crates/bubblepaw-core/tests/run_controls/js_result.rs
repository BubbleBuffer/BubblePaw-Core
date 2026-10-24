use super::*;

struct Js;
#[async_trait]
impl Tool for Js {
    fn definition(&self) -> ToolDefinition {
        let mut tool = Counter::default().definition();
        tool.name = "js".into();
        tool
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
        dispatcher.invoke(context, "submit_result", input).await
    }
}

struct CodeSurface(Arc<Scripted>);
#[async_trait]
impl Provider for CodeSurface {
    fn capabilities(&self, _: &str) -> Capabilities {
        Capabilities {
            tool_surface: ToolSurface::Code,
            ..Default::default()
        }
    }
    async fn generate(
        &self,
        request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        self.0.generate(request, on_text, cancellation).await
    }
}

#[tokio::test]
async fn result_nudge_preserves_the_original_provider_tool_surface() -> Result<()> {
    let mut result = submit(json!({"count":1}));
    result.tool_calls[0].name = "js".into();
    let provider = Scripted::new(vec![reply("bad"), result]);
    let store = Store::in_memory()?;
    let session = store.create_session("code surface")?.id;
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Counter::default()))?;
    tools.register(Arc::new(Js))?;
    let harness = Harness::new(
        store.clone(),
        Arc::new(CodeSurface(provider.clone())),
        tools,
        HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            ..Default::default()
        },
    );
    let run = harness
        .prompt(
            &session,
            "work",
            RunOptions {
                output_schema: Some(schema()),
                max_tool_calls: Some(2),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].tools.len(), 1);
    assert_eq!(requests[0].tools[0].name, "js");
    assert!(requests[1].disabled_tools.is_empty());
    assert_eq!(
        serde_json::to_value(&requests[1].tools)?,
        serde_json::to_value(&requests[0].tools)?
    );
    assert_eq!(requests[0].system_prompt, requests[1].system_prompt);
    assert_eq!(budget_notices(&store.messages(&session)?).len(), 1);
    Ok(())
}
