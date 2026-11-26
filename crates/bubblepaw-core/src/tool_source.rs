//! What the run loop asks about the tools a run offers. Each method is one
//! question or report named by the loop's need; the implementation owns the
//! saved surface, the inventory and the notes that following it needs. The loop
//! decides when to ask and what to do with the answer.
use crate::loop_options::LoopOptions;
use crate::round_reminders::{RunReminders, ToolsRound};
use crate::run_data::Run;
use crate::tool_registry::ToolRegistry;
use crate::tool_result::ToolResult;
use anyhow::Result;
use bubblepaw_ai::{ModelRequest, ToolDefinition};

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
    fn open_run(&self, run: &Run, options: &LoopOptions) -> Result<RunTools>;
    /// The tools for this round, after the host's registry may have changed.
    fn round_tools(&self, run: &Run, options: &LoopOptions) -> Result<ToolRegistry>;
    /// The host may have changed what it serves. When it did, `tools`, `surface`
    /// and the request are brought up to date and this returns true.
    fn surface_changed(
        &self,
        run: &Run,
        options: &LoopOptions,
        tools: &mut ToolRegistry,
        surface: &mut ToolSurface,
        request: &mut ModelRequest,
    ) -> Result<bool>;
    /// Queue the notices for the next request and say whether tools are open in it.
    /// `remaining_calls` is what is left of the run's tool budget, if it has one.
    fn queue_round_reminders(
        &self,
        run: &Run,
        options: &LoopOptions,
        remaining_calls: Option<usize>,
        state: &mut RunReminders,
    ) -> ToolsRound;
    /// A tool result held to the size the model may read, with the budget footer.
    fn bound_result(&self, result: ToolResult, remaining_calls: Option<usize>) -> ToolResult;
    /// The model replied without the result the run requires. The first time this
    /// queues the one reminder; the second time it is an error.
    fn result_required(&self, run: &Run, nudged: &mut bool) -> Result<()>;
}
