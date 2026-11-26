//! The one `ToolSource`, over the harness's store, tool selection and config.
use super::*;
use crate::loop_options::LoopOptions;
use crate::round_reminders::{RunReminders, ToolsRound};
use crate::tool_source::{RunTools, ToolSource, ToolSurface};

/// The tools of one run. It holds the run's own options: the loop's options carry
/// only what the loop reads, and choosing the tools takes more.
pub(super) struct StockToolSource<'a> {
    harness: &'a Harness,
    options: &'a RunOptions,
}

impl<'a> StockToolSource<'a> {
    pub(super) fn new(harness: &'a Harness, options: &'a RunOptions) -> Self {
        Self { harness, options }
    }
}

impl ToolSource for StockToolSource<'_> {
    fn open_run(&self, run: &Run, _options: &LoopOptions) -> Result<RunTools> {
        let (harness, options) = (self.harness, self.options);
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

    fn round_tools(&self, run: &Run, _options: &LoopOptions) -> Result<ToolRegistry> {
        self.harness
            .refresh_run_tools(&run.session_id, self.options)
    }

    fn surface_changed(
        &self,
        run: &Run,
        _options: &LoopOptions,
        tools: &mut ToolRegistry,
        surface: &mut ToolSurface,
        request: &mut ModelRequest,
    ) -> Result<bool> {
        self.harness.apply_tool_surface_change(
            &run.session_id,
            self.options,
            tools,
            surface,
            request,
        )
    }

    fn queue_round_reminders(
        &self,
        run: &Run,
        _options: &LoopOptions,
        remaining_calls: Option<usize>,
        state: &mut RunReminders,
    ) -> ToolsRound {
        self.harness
            .queue_run_reminders(run, self.options, remaining_calls, state)
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
