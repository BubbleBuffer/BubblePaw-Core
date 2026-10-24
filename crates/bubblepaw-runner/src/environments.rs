//! Host registration for volatile execution environment controls.
//! This capability can be installed independently of the JavaScript entrypoint.
use crate::codemode::CodeMode;
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{Tool, ToolContext, ToolDefinition, ToolRegistry, ToolResult};
use serde_json::{Value, json};
use std::sync::Arc;

/// Register host environment controls and large-result preservation without
/// exposing the JavaScript model entrypoint.
pub fn register(mode: &Arc<CodeMode>, registry: &mut ToolRegistry) -> Result<()> {
    mode.spill_store().register(registry);
    if registry.get_host("environments").is_none() {
        registry.register_host(Arc::new(EnvironmentTool(mode.clone())))?;
    }
    Ok(())
}

struct EnvironmentTool(Arc<CodeMode>);
#[async_trait]
impl Tool for EnvironmentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            approval: Default::default(),
            name: "environments".into(),
            description: "Inspect, create, reset, close, cancel, background or wake named volatile execution environments. Busy reset/close require cancel:true. State inspection shows bounded keys, types and primitive previews without invoking getters. Limits: 4 per session, 16 total; 32 MiB heap, 3 seconds active execution, 30 seconds non-wait idle per cell.".into(),
            read_only: true,
            input_schema: json!({"type":"object","properties":{"action":{"enum":["inspect","create","reset","close","cancel","background","wake"]},"environment":{"type":"string"},"cancel":{"type":"boolean"}},"required":["action"],"additionalProperties":false}),
        }
    }
    async fn invoke(&self, context: ToolContext, input: Value) -> Result<ToolResult> {
        let content = self
            .0
            .control(&context.session_id, input.clone())?
            .to_string();
        Ok(ToolResult::text(
            self.0
                .inspected_images(&context.session_id, &input)
                .attach(content),
        ))
    }
}
