//! What the run loop asks about the tools a run offers. Each method is one
//! question or report named by the loop's need; the implementation owns the
//! saved surface, the inventory and the notes that following it needs. The loop
//! decides when to ask and what to do with the answer.
use super::runtime_reminders::{RunReminders, ToolsRound};
use super::*;
use crate::ToolDefinition;

/// The system prompt and tool declarations a run sends with every request.
pub(crate) struct ToolSurface {
    pub(crate) system_prompt: String,
    pub(crate) declarations: Vec<ToolDefinition>,
}

impl ToolSurface {
    /// Put the surface back on a request a host edited; these are run controls.
    pub(crate) fn restore(&self, request: &mut ModelRequest) {
        request.system_prompt = self.system_prompt.clone();
        request.tools = self.declarations.clone();
        request.disabled_tools.clear();
        request.output_schema = None;
    }
}

/// The tools a run starts with.
pub(crate) struct RunTools {
    pub(crate) tools: ToolRegistry,
    pub(crate) surface: ToolSurface,
    /// Whether every round asks for the tools again, rather than only after a change.
    pub(crate) refresh_each_round: bool,
}

pub(crate) trait ToolSource: Send + Sync {
    /// The tools, the surface and the refresh rule a run starts with.
    fn open_run(&self, run: &Run, options: &RunOptions) -> Result<RunTools>;
    /// The tools for this round, after the host's registry may have changed.
    fn round_tools(&self, run: &Run, options: &RunOptions) -> Result<ToolRegistry>;
    /// The host may have changed what it serves. When it did, `tools`, `surface`
    /// and the request are brought up to date and this returns true.
    fn surface_changed(
        &self,
        run: &Run,
        options: &RunOptions,
        tools: &mut ToolRegistry,
        surface: &mut ToolSurface,
        request: &mut ModelRequest,
    ) -> Result<bool>;
    /// Queue the notices for the next request and say whether tools are open in it.
    /// `remaining_calls` is what is left of the run's tool budget, if it has one.
    fn queue_round_reminders(
        &self,
        run: &Run,
        options: &RunOptions,
        remaining_calls: Option<usize>,
        state: &mut RunReminders,
    ) -> ToolsRound;
    /// A tool result held to the size the model may read, with the budget footer.
    fn bound_result(&self, result: ToolResult, remaining_calls: Option<usize>) -> ToolResult;
    /// The model replied without the result the run requires. The first time this
    /// queues the one reminder; the second time it is an error.
    fn result_required(&self, run: &Run, nudged: &mut bool) -> Result<()>;
}

/// The one implementation, over the harness's store, tool selection and config.
pub(super) struct StockToolSource<'a> {
    harness: &'a Harness,
}

impl<'a> StockToolSource<'a> {
    pub(super) fn new(harness: &'a Harness) -> Self {
        Self { harness }
    }
}

impl ToolSource for StockToolSource<'_> {
    fn open_run(&self, run: &Run, options: &RunOptions) -> Result<RunTools> {
        let harness = self.harness;
        let tools = harness.run_tool_snapshot(&run.session_id, options)?;
        let refresh_each_round =
            harness.execution_mode(&run.session_id)? == crate::ExecutionMode::Native;
        let surface = harness.run_surface(&run.session_id, options)?;
        harness.queue_result_schema_notice(run, options, &tools)?;
        Ok(RunTools {
            tools,
            surface,
            refresh_each_round,
        })
    }

    fn round_tools(&self, run: &Run, options: &RunOptions) -> Result<ToolRegistry> {
        self.harness.refresh_run_tools(&run.session_id, options)
    }

    fn surface_changed(
        &self,
        run: &Run,
        options: &RunOptions,
        tools: &mut ToolRegistry,
        surface: &mut ToolSurface,
        request: &mut ModelRequest,
    ) -> Result<bool> {
        self.harness
            .apply_tool_surface_change(&run.session_id, options, tools, surface, request)
    }

    fn queue_round_reminders(
        &self,
        run: &Run,
        options: &RunOptions,
        remaining_calls: Option<usize>,
        state: &mut RunReminders,
    ) -> ToolsRound {
        self.harness
            .queue_run_reminders(run, options, remaining_calls, state)
    }

    fn bound_result(&self, result: ToolResult, remaining_calls: Option<usize>) -> ToolResult {
        self.harness.bound_tool_result(result, remaining_calls)
    }

    fn result_required(&self, run: &Run, nudged: &mut bool) -> Result<()> {
        self.harness.require_result(run, nudged)
    }
}

impl Harness {
    /// The stock gate over the store, built for one run of the loop.
    pub(super) fn stock_gate(&self) -> Arc<dyn crate::tool_gate::ToolGate> {
        Arc::new(crate::approvals::StockGate::new(
            self.store.clone(),
            self.config.auto_approval.clone(),
            self.config.workspace_resolver.clone(),
        ))
    }
}
