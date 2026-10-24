use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    ApprovalMode, ApprovalPolicy, CancellationToken, Harness, HarnessConfig, ModelRequest,
    ModelResponse, Provider, Role, RunOptions, Store, TextCallback, Tool, ToolCall, ToolContext,
    ToolDefinition, ToolRegistry, ToolResult,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

struct CallTool;
#[async_trait]
impl Provider for CallTool {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(ModelResponse {
            text: "done".into(),
            tool_calls: if request
                .messages
                .last()
                .is_some_and(|m| m.role == Role::User)
            {
                vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "inspect".into(),
                    arguments: json!({}),
                }]
            } else {
                vec![]
            },
            ..Default::default()
        })
    }
}

#[derive(Default)]
struct Inspect(Mutex<Vec<ApprovalPolicy>>);
#[async_trait]
impl Tool for Inspect {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "inspect".into(),
            description: "Record the policy exposed to tools".into(),
            input_schema: json!({"type":"object"}),
            read_only: true,
            approval: Default::default(),
        }
    }
    async fn invoke(&self, context: ToolContext, _: Value) -> Result<ToolResult> {
        self.0.lock().unwrap().push(context.approval);
        Ok(ToolResult::text("recorded"))
    }
}

#[tokio::test]
async fn tools_receive_run_override_or_harness_approval_policy() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("approval context")?.id;
    let tool = Arc::new(Inspect::default());
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone())?;
    let default_policy = ApprovalPolicy {
        confirm: ApprovalMode::Deny,
        tools: [("delegated".into(), ApprovalMode::Ask)].into(),
        timeout_secs: Some(11),
        ..Default::default()
    };
    let override_policy = ApprovalPolicy {
        confirm: ApprovalMode::Allow,
        tools: [("delegated".into(), ApprovalMode::Deny)].into(),
        timeout_secs: Some(23),
        ..Default::default()
    };
    let harness = Harness::new(
        store,
        Arc::new(CallTool),
        tools,
        HarnessConfig {
            approval: default_policy.clone(),
            ..Default::default()
        },
    );
    for approval in [None, Some(override_policy.clone()), None] {
        let run = harness
            .prompt(
                &session,
                "inspect policy",
                RunOptions {
                    approval,
                    ..Default::default()
                },
            )
            .await?;
        assert_eq!(run.status, "completed", "{:?}", run.error);
    }
    assert_eq!(
        *tool.0.lock().unwrap(),
        vec![default_policy.clone(), override_policy, default_policy]
    );
    Ok(())
}
