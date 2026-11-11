//! SCRIPTED dispatch regressions: rejected spawns must not become unknown effects.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, Role,
    RunOptions, Store, TextCallback, Tool, ToolCall, ToolContext, ToolDefinition, ToolDispatcher,
    ToolRegistry, ToolResult,
};
use bubblepaw_runner::{agents::AgentTools, config::Agents};
use serde_json::{Value, json};
use std::sync::Arc;

struct RejectionProvider;
#[async_trait]
impl Provider for RejectionProvider {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let (index, user) = request
            .messages
            .iter()
            .enumerate()
            .rev()
            .find(|(_, message)| message.role == Role::User)
            .unwrap();
        if user.content == "block" {
            cancellation.cancelled().await;
            anyhow::bail!("scripted cancellation");
        }
        if !request.messages[index + 1..]
            .iter()
            .any(|message| message.role == Role::Tool)
        {
            return Ok(ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "bridge".into(),
                    arguments: serde_json::from_str(&user.content)?,
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

struct Bridge;
#[async_trait]
impl Tool for Bridge {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bridge".into(),
            description: "scripted host call".into(),
            input_schema: json!({"type":"object"}),
            read_only: true,
            approval: Default::default(),
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        unreachable!("the harness supplies its dispatcher")
    }
    async fn invoke_with_dispatcher(
        &self,
        context: ToolContext,
        input: Value,
        dispatcher: ToolDispatcher,
    ) -> Result<ToolResult> {
        let result = dispatcher.invoke(context, "agent", input).await?;
        // The outer model entrypoint returns the admitted handle immediately;
        // only the nested spawn remains pending until its child settles.
        Ok(ToolResult {
            pending: false,
            ..result
        })
    }
}

async fn rejected_spawn(input: Value, fill_slots: bool) -> Result<()> {
    let store = Store::in_memory()?;
    let parent = store.create_session("rejection regression")?;
    let provider = Arc::new(RejectionProvider);
    let child = Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    // The concurrency case depends on the two-active-child cap, now explicit.
    let agents = Arc::new(
        AgentTools::new(store.clone(), child, None).with_limits(Agents {
            max_active_per_parent: Some(2),
            ..Default::default()
        }),
    );
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Bridge))?;
    agents.register(&mut tools)?;
    let harness = Harness::new(store.clone(), provider, tools, HarnessConfig::default());
    if fill_slots {
        for _ in 0..2 {
            harness
                .prompt(
                    &parent.id,
                    json!({"task":"block"}).to_string(),
                    RunOptions::default(),
                )
                .await?;
        }
    }
    let run = harness
        .prompt(&parent.id, input.to_string(), RunOptions::default())
        .await?;
    assert_eq!(run.status, "completed");
    let messages = store.messages(&parent.id)?;
    let rejected = messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Tool)
        .unwrap();
    assert!(
        rejected.is_error,
        "ordinary rejection must reach the provider as an error: {}",
        rejected.content
    );
    agents.shutdown().await?;
    let unresolved = store.pending_effects(&parent.id)?;
    assert!(
        unresolved.is_empty(),
        "rejected spawn has no external outcome to recover: {unresolved:?}"
    );
    Ok(())
}

#[tokio::test]
async fn oversized_spawn_rejection_does_not_leave_unknown_effect() -> Result<()> {
    rejected_spawn(
        json!({"task":"too much", "context":"x".repeat(64 * 1024)}),
        false,
    )
    .await
}

#[tokio::test]
async fn unavailable_profile_rejection_does_not_leave_unknown_effect() -> Result<()> {
    rejected_spawn(
        json!({"task":"unknown profile", "profile":"missing"}),
        false,
    )
    .await
}

#[tokio::test]
async fn concurrency_rejection_does_not_leave_unknown_effect() -> Result<()> {
    rejected_spawn(json!({"task":"third child"}), true).await
}
