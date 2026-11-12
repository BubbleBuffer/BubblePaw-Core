use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Script {
    round: AtomicUsize,
    fail: bool,
}
#[async_trait]
impl Provider for Script {
    async fn generate(
        &self,
        _: ModelRequest,
        text: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        if self.fail {
            bail!("provider-private-failure");
        }
        text("hello");
        let round = self.round.fetch_add(1, Ordering::Relaxed);
        Ok(ModelResponse {
            text: if round == 0 {
                String::new()
            } else {
                "done-private-text".into()
            },
            tool_calls: if round == 0 {
                vec![ToolCall {
                    id: "outer".into(),
                    name: "composite".into(),
                    arguments: json!({}),
                }]
            } else {
                vec![]
            },
            usage: Some(Usage {
                input_tokens: if round == 0 { 12 } else { 3 },
                output_tokens: if round == 0 { 7 } else { 2 },
                ..Default::default()
            }),
            provider_state: Some(json!({"authorization":"provider-private-state"})),
            last_request_usage: None,
            quota: Vec::new(),
        })
    }
}
struct Nested;
#[async_trait]
impl Tool for Nested {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            approval: Default::default(),
            name: "exec".into(),
            description: "fake host".into(),
            input_schema: json!({"type":"object","required":["command"]}),
            read_only: true,
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        Ok(ToolResult::text(
            json!({"status":"exited","handle":"process:fake","exitCode":9,"stdout":"private-stdout-data","stderr":"private-stderr-data","capture":{"stdout":{"totalBytes":101,"gaps":[{"from":1,"to":2},{"from":3,"to":5}],"captureError":true},"stderr":{"totalBytes":23,"gaps":[],"captureError":false}},"headers":{"Authorization":"tool-private-credential"}}).to_string(),
        ))
    }
}
struct Composite;
#[async_trait]
impl Tool for Composite {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            approval: Default::default(),
            name: "composite".into(),
            description: "composition".into(),
            input_schema: json!({"type":"object"}),
            read_only: true,
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        unreachable!()
    }
    async fn invoke_with_dispatcher(
        &self,
        context: ToolContext,
        _: Value,
        dispatcher: ToolDispatcher,
    ) -> Result<ToolResult> {
        let dispatcher = dispatcher.with_execution("cell-42", "main", 7);
        for _ in 0..2 {
            dispatcher
                .invoke(
                    context.clone(),
                    "exec",
                    json!({"command":"echo private-command"}),
                )
                .await?;
        }
        dispatcher.invoke(context.clone(), "exec", json!({"command":{"parts":["echo ",""],"values":["private-value"]},"authorization":"input-private-credential"})).await?;
        // Shared dispatch reports invalid tool input as a correctable result,
        // while transport/storage failures remain Err. Keep this error observed.
        let rejected = dispatcher
            .invoke(context.clone(), "exec", json!({"wrong":true}))
            .await?;
        anyhow::ensure!(rejected.is_error, "invalid nested arguments were accepted");
        anyhow::ensure!(
            rejected.content.contains("Invalid arguments for exec"),
            "nested dispatch returned the wrong rejection: {}",
            rejected.content
        );
        dispatcher.emit_output(
            &context,
            "cell-42",
            json!({"type":"execution","status":"failed","error":"private-output-error"}),
        )?;
        Ok(ToolResult::text("composition done"))
    }
}
pub(super) async fn run(
    trace: Option<&JsonlTrace>,
    fail: bool,
) -> Result<(tempfile::TempDir, Run)> {
    let directory = tempfile::tempdir()?;
    let store = Store::open(directory.path().join("archive.sqlite"))?;
    let session = store.create_session("trace test")?;
    let mut registry = ToolRegistry::new();
    registry.register_host(Arc::new(Nested))?;
    registry.register(Arc::new(Composite))?;
    let harness = Harness::new(
        store,
        Arc::new(Script {
            round: AtomicUsize::new(0),
            fail,
        }),
        registry,
        HarnessConfig {
            observation: trace.map(JsonlTrace::observation),
            ..Default::default()
        },
    );
    let result = match tokio::time::timeout(
        Duration::from_secs(10),
        harness.prompt(&session.id, "private-prompt", RunOptions::default()),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            harness.cancel(&session.id);
            let cleanup = harness.shutdown(Duration::from_secs(2)).await;
            bail!(
                "tracing fixture did not finish within 10 seconds; cancellation/shutdown: {cleanup:?}"
            );
        }
    };
    harness.shutdown(Duration::from_secs(2)).await?;
    Ok((directory, result))
}
