//! Shared budgets for nested native composition.
use super::*;

struct Compose;
#[async_trait]
impl Tool for Compose {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "compose".into(),
            description: "batch host calls".into(),
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
        context: ToolContext,
        _: Value,
        dispatcher: ToolDispatcher,
    ) -> Result<ToolResult> {
        let mut jobs = tokio::task::JoinSet::new();
        for _ in 0..6 {
            let dispatcher = dispatcher.clone();
            let context = context.clone();
            jobs.spawn(async move {
                let result = dispatcher.invoke(context, "count", json!({})).await?;
                Ok::<_, anyhow::Error>(result)
            });
        }
        let mut allowed = 0;
        let mut denied = 0;
        while let Some(result) = jobs.join_next().await {
            let result = result??;
            if result.is_error {
                assert!(result.content.contains("budget exhausted"));
                denied += 1;
            } else {
                assert_eq!(serde_json::from_str::<Value>(&result.content)?["ok"], true);
                allowed += 1;
            }
        }
        Ok(ToolResult::text(
            json!({"allowed":allowed,"denied":denied}).to_string(),
        ))
    }
}

#[tokio::test]
async fn composition_wrapper_and_parallel_nested_host_calls_share_one_budget() -> Result<()> {
    let counter = Arc::new(Counter::default());
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Compose))?;
    tools.register_host(counter.clone())?;
    let provider = Scripted::new(vec![calls("compose", 1), reply("finished")]);
    let (harness, store, session) = harness(provider.clone(), tools)?;
    let run = harness
        .prompt(
            &session,
            "work",
            RunOptions {
                max_tool_calls: Some(3),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(counter.0.load(Ordering::Relaxed), 2);
    assert!(!provider.requests.lock().unwrap()[1].tools.is_empty());
    let result = store
        .messages(&session)?
        .into_iter()
        .find(|m| m.role == Role::Tool)
        .unwrap();
    assert!(
        result.content.starts_with(r#"{"allowed":2,"denied":4}"#),
        "{}",
        result.content
    );
    assert!(result.content.ends_with("0 tool calls left"));
    Ok(())
}
