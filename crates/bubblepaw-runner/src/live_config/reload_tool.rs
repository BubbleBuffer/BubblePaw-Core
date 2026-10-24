//! Shared reload-tool registration for live hosts and declaration-only snapshots.
use super::LiveConfig;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use bubblepaw_core::{Tool, ToolContext, ToolDefinition, ToolDispatcher, ToolRegistry};
use serde_json::{Value, json};
use std::sync::{Arc, Weak};

pub(crate) fn register_reload_tool(
    registry: &mut ToolRegistry,
    host: Weak<LiveConfig>,
) -> Result<()> {
    registry.register_host(Arc::new(ReloadTool(host)))?;
    registry.show_always(["reload"]);
    Ok(())
}

struct ReloadTool(Weak<LiveConfig>);
#[async_trait]
impl Tool for ReloadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "reload".into(),
            description: "Apply config edits now; returns the diff. Optional server restarts that MCP server even if unchanged. New tools are available next model round.".into(),
            input_schema: json!({"type":"object","properties":{"server":{"type":"string","minLength":1}},"additionalProperties":false}),
            read_only: true, approval: Default::default(),
        }
    }
    async fn invoke(
        &self,
        _context: ToolContext,
        input: Value,
    ) -> Result<bubblepaw_core::ToolResult> {
        let live = self
            .0
            .upgrade()
            .ok_or_else(|| anyhow!("Config host was released"))?;
        let report = live.reload(input["server"].as_str()).await;
        Ok(bubblepaw_core::ToolResult {
            content: serde_json::to_string(&report)?,
            is_error: !report.applied || !report.errors.is_empty(),
            pending: false,
        })
    }
    async fn invoke_with_dispatcher(
        &self,
        context: ToolContext,
        input: Value,
        dispatcher: ToolDispatcher,
    ) -> Result<bubblepaw_core::ToolResult> {
        let result = self.invoke(context, input).await?;
        dispatcher.refresh_tools_next_round();
        Ok(result)
    }
    fn take_reminders(&self, session: &str) -> Vec<bubblepaw_core::Reminder> {
        self.0
            .upgrade()
            .map(|live| live.take_reminders(session))
            .unwrap_or_default()
    }
}
