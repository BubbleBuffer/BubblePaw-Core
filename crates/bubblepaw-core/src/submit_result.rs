//! Structured completion is an ordinary validated tool call, settled after its batch.
use crate::{
    Approval, Tool, ToolContext, ToolDefinition, ToolDispatcher, ToolRegistry, ToolResult,
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

pub(crate) const NAME: &str = "submit_result";

pub(crate) fn is_submission(call: &crate::ToolCall) -> bool {
    call.name == NAME
        || (call.name == "call_tool"
            && call.arguments.get("name").and_then(Value::as_str) == Some(NAME))
}

struct SubmitResult {
    schema: Value,
}

pub(crate) fn register(registry: &mut ToolRegistry, schema: &Value) -> Result<()> {
    // The name is reserved by the engine for schema-constrained runs. A host
    // capability cannot replace the run's result contract.
    registry.remove(NAME, "reserved for this run's structured result");
    registry.register_host(Arc::new(SubmitResult {
        schema: schema.clone(),
    }))?;
    registry.show_always([NAME]);
    if let Some(allowed) = &mut registry.model_allowlist {
        allowed.push(NAME.into());
    }
    Ok(())
}

/// A host context projection may replace its disposable request. The completion
/// contract is run-owned and stays last in native declarations on every request.
pub(crate) fn restore_declaration(
    request: &mut crate::ModelRequest,
    schema: &Value,
    mode: crate::ExecutionMode,
) {
    request.output_schema = None;
    request.disabled_tools.retain(|tool| tool.name != NAME);
    if mode == crate::ExecutionMode::Native {
        request.tools.retain(|tool| tool.name != NAME);
        request.tools.push(
            SubmitResult {
                schema: schema.clone(),
            }
            .definition(),
        );
    }
}

#[async_trait]
impl Tool for SubmitResult {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: NAME.into(),
            description: "Submit the required structured result to complete this run. Arguments must satisfy this exact schema. Submit once; the first valid result is kept. Other tool calls in the same batch finish before the run completes.".into(),
            input_schema: self.schema.clone(),
            approval: Approval::None,
            read_only: true,
        }
    }

    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        anyhow::bail!("submit_result requires the host tool dispatcher")
    }

    async fn invoke_with_dispatcher(
        &self,
        _: ToolContext,
        input: Value,
        dispatcher: ToolDispatcher,
    ) -> Result<ToolResult> {
        Ok(dispatcher.accept_result(input))
    }
}

impl ToolDispatcher {
    fn accept_result(&self, input: Value) -> ToolResult {
        let mut result = self.submitted_result.lock().unwrap();
        if result.is_some() {
            return ToolResult::error("submit_result already accepted a result for this run");
        }
        *result = Some(input);
        ToolResult::text("Structured result accepted; completing after this tool batch.")
    }

    pub(crate) fn take_submitted_result(&self) -> Option<Value> {
        self.submitted_result.lock().unwrap().take()
    }
}
