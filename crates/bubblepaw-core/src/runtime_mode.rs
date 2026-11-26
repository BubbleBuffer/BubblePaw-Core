//! Select each chat's persisted execution surface for runs and introspection.
use super::*;

/// Which tools a session sees: the host's registry in the session's execution mode,
/// narrowed by the run's options. Cheap to clone.
#[derive(Clone)]
pub(super) struct ToolSelection {
    store: Store,
    tools: ToolRegistry,
    resolver: Option<super::ToolRegistryResolver>,
    default_mode: crate::ExecutionMode,
}

impl ToolSelection {
    pub(super) fn new(
        store: Store,
        tools: ToolRegistry,
        resolver: Option<super::ToolRegistryResolver>,
        default_mode: crate::ExecutionMode,
    ) -> Self {
        Self {
            store,
            tools,
            resolver,
            default_mode,
        }
    }

    fn snapshot_in_mode(&self, mode: crate::ExecutionMode) -> ToolRegistry {
        self.resolver
            .as_ref()
            .map(|resolve| resolve())
            .unwrap_or_else(|| self.tools.clone())
            .with_execution_mode(mode)
    }

    pub(super) fn selected(&self, session_id: &str, options: &RunOptions) -> Result<ToolRegistry> {
        let mode = self
            .store
            .resolve_execution_mode(session_id, self.default_mode)?;
        self.selected_in_mode(session_id, options, mode)
    }

    pub(super) fn selected_in_mode(
        &self,
        session_id: &str,
        options: &RunOptions,
        mode: crate::ExecutionMode,
    ) -> Result<ToolRegistry> {
        let tools = self
            .snapshot_in_mode(mode)
            .with_model_allowlist(options.tool_names.as_deref());
        let mut tools = if self.store.session(session_id)?.read_only {
            tools.read_only()
        } else {
            tools
        };
        if let Some(schema) = &options.output_schema {
            crate::submit_result::register(&mut tools, &schema.schema)?;
            tools = tools.with_execution_mode(mode);
        } else {
            tools.remove(
                crate::submit_result::NAME,
                "submit_result requires a run output schema",
            );
        }
        Ok(tools)
    }
}

impl Harness {
    pub(super) fn tool_snapshot(&self, session_id: &str) -> Result<ToolRegistry> {
        let mode = self.execution_mode(session_id)?;
        Ok(self.selection.snapshot_in_mode(mode))
    }

    pub(super) fn selected_tools(
        &self,
        session_id: &str,
        options: &RunOptions,
    ) -> Result<ToolRegistry> {
        self.selection.selected(session_id, options)
    }

    pub(super) fn selected_tools_in_mode(
        &self,
        session_id: &str,
        options: &RunOptions,
        mode: crate::ExecutionMode,
    ) -> Result<ToolRegistry> {
        self.selection.selected_in_mode(session_id, options, mode)
    }

    /// The session's immutable persisted mode; new chats bind once to the host default.
    pub fn execution_mode(&self, session_id: &str) -> Result<crate::ExecutionMode> {
        self.store
            .resolve_execution_mode(session_id, self.config.execution_mode)
    }

    /// The initial declarations selected for this session and run, including
    /// dynamic tool help and the provider's tool surface. `tool_names` filters
    /// adapter help and discovery; native declarations keep the profile's tools.
    /// Approval-denied tools remain declared, exactly as in ordinary model turns;
    /// their calls still pass through the approval gate.
    pub fn effective_tool_definitions(
        &self,
        session_id: &str,
        options: &RunOptions,
    ) -> Result<Vec<crate::ToolDefinition>> {
        Ok(self.chat_surface(session_id, options, false)?.declarations)
    }
}
