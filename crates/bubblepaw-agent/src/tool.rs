//! The tool trait and the context of one call.
use crate::dispatch::ToolDispatcher;
use crate::extensions::Extensions;
use crate::reminder_types::Reminder;
use crate::tool_registry::ToolRegistry;
use crate::tool_result::ToolResult;
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_ai::{CancellationToken, ToolDefinition};
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct ToolContext {
    pub session_id: String,
    pub run_id: String,
    pub invocation_id: String,
    pub cancellation: CancellationToken,
    /// Typed values the shell attaches to a call, read back through accessors on the
    /// types the shell stores.
    pub extensions: Extensions,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    /// The definition as advertised from a given registry. Tools whose
    /// description depends on the registry's current capabilities (`js`)
    /// override this; the result must be deterministic for the same registry.
    fn definition_in(&self, _registry: &ToolRegistry) -> ToolDefinition {
        self.definition()
    }
    async fn invoke(&self, context: ToolContext, input: Value) -> Result<ToolResult>;
    async fn invoke_with_dispatcher(
        &self,
        context: ToolContext,
        input: Value,
        _dispatcher: ToolDispatcher,
    ) -> Result<ToolResult> {
        self.invoke(context, input).await
    }
    /// Explicit host control may release a foreground wait without cancelling work.
    fn interrupt_wait(&self, _session_id: &str) -> usize {
        0
    }
    /// Drain session-owned reminders for the next model request. Reminders
    /// keep their delivery identities through coalescing and recovery.
    fn take_reminders(&self, _session_id: &str) -> Vec<Reminder> {
        Vec::new()
    }
    fn cancel_session(&self, _session_id: &str) -> usize {
        0
    }
}
