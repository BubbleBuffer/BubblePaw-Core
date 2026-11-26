//! What the run loop asks about the tools a run offers. Each method is one
//! question or report named by the loop's need; the implementation owns the
//! saved surface, the inventory and the notes that following it needs. The loop
//! decides when to ask and what to do with the answer.
use crate::loop_options::LoopOptions;
use crate::round_reminders::{RunReminders, ToolsRound};
use crate::run_data::Run;
use crate::tool_registry::ToolRegistry;
use crate::tool_result::ToolResult;
use anyhow::{Result, bail};
use bubblepaw_ai::{ModelRequest, ToolDefinition};

/// The system prompt and tool declarations a run sends with every request.
pub struct ToolSurface {
    pub system_prompt: String,
    pub declarations: Vec<ToolDefinition>,
}

impl ToolSurface {
    /// Put the surface back on a request a host edited; these are run controls.
    pub fn restore(&self, request: &mut ModelRequest) {
        request.system_prompt = self.system_prompt.clone();
        request.tools = self.declarations.clone();
        request.disabled_tools.clear();
        request.output_schema = None;
    }
}

/// The tools a run starts with.
pub struct RunTools {
    pub tools: ToolRegistry,
    pub surface: ToolSurface,
    /// Whether every round asks for the tools again, rather than only after a change.
    pub refresh_each_round: bool,
}

/// Required methods give the loop the tools and the size-bound results it sends;
/// defaulted ones are notices and refresh rules a fixed tool set never needs.
pub trait ToolSource: Send + Sync {
    /// The tools, the surface and the refresh rule a run starts with.
    fn open_run(&self, run: &Run, options: &LoopOptions) -> Result<RunTools>;
    /// The tools for this round, after the host's registry may have changed.
    fn round_tools(&self, run: &Run, options: &LoopOptions) -> Result<ToolRegistry>;
    /// A tool result held to the size the model may read, with the budget footer.
    fn bound_result(&self, result: ToolResult, remaining_calls: Option<usize>) -> ToolResult;
    /// Override when the host's served tools can change during a run: bring `tools`,
    /// `surface` and the request up to date and return true. The default says nothing changed.
    fn surface_changed(
        &self,
        _run: &Run,
        _options: &LoopOptions,
        _tools: &mut ToolRegistry,
        _surface: &mut ToolSurface,
        _request: &mut ModelRequest,
    ) -> Result<bool> {
        Ok(false)
    }
    /// Override to queue notices (tool budget, restrictions) for the next request and
    /// say whether tools are open in it. The default queues none and tools are open.
    fn queue_round_reminders(
        &self,
        _run: &Run,
        _options: &LoopOptions,
        _remaining_calls: Option<usize>,
        _state: &mut RunReminders,
    ) -> ToolsRound {
        ToolsRound::Open
    }
    /// Override to queue a reminder on the first reply without the required result.
    /// The default queues none: the first such reply is tolerated and the second is an error.
    fn result_required(&self, _run: &Run, nudged: &mut bool) -> Result<()> {
        if std::mem::replace(nudged, true) {
            bail!("The model replied without the required result");
        }
        Ok(())
    }
}
