//! What the run loop is built from and what one run hands it. The loop holds
//! seams and plain values only.
use crate::dispatch::ToolDispatcher;
use crate::live_events::LiveEvents;
use crate::loop_hooks::{ProviderResolver, TurnStart};
use crate::loop_options::LoopOptions;
use crate::loop_records::SingleToolRun;
use crate::observer::Observer;
use crate::request_seam::RequestContext;
use crate::run_data::Run;
use crate::run_journal::RunJournal;
use crate::tool_registry::ToolRegistry;
use crate::tool_source::ToolSource;
use bubblepaw_ai::{CancellationToken, Message, Usage};
use std::sync::Arc;

/// The plain config values the loop reads.
#[derive(Clone)]
pub(crate) struct LoopLimits {
    pub(crate) turn_start: Option<Arc<dyn TurnStart>>,
    /// The default round limit; a run's own option overrides it.
    pub(crate) max_rounds: Option<usize>,
    pub(crate) max_request_bytes: usize,
    pub(crate) max_paragraph_bytes: usize,
}

/// The loop for one session shell: the seams it writes and asks through, and its limits.
pub(crate) struct RunLoop {
    pub(crate) journal: Arc<dyn RunJournal>,
    pub(crate) live: Arc<dyn LiveEvents>,
    pub(crate) request_context: Arc<dyn RequestContext>,
    /// The default model and the request provider, asked afresh each round.
    pub(crate) provider: ProviderResolver,
    pub(crate) observation: Option<Arc<dyn Observer>>,
    pub(crate) limits: LoopLimits,
}

/// Builds the dispatcher a run starts with, over its first tools.
pub(crate) type NewDispatcher = dyn Fn(ToolRegistry) -> ToolDispatcher + Send + Sync;

/// What one run needs to start: the run, its options and how it is stopped.
#[derive(Clone, Copy)]
pub(crate) struct RunInputs<'a> {
    pub(crate) run: &'a Run,
    pub(crate) options: &'a LoopOptions,
    pub(crate) cancellation: &'a CancellationToken,
    pub(crate) single_tool: Option<&'a SingleToolRun>,
    /// The tools each round offers.
    pub(crate) tools: &'a dyn ToolSource,
    /// A fully configured dispatcher over the tools a run starts with, built once per run.
    pub(crate) dispatcher: &'a NewDispatcher,
    /// Whether the gate is told the user's latest request, kept current as steering arrives.
    pub(crate) wants_user_request: bool,
}

/// How a run ended, for the shell to record on the run and commit.
pub(crate) struct RunOutcome {
    pub(crate) status: String,
    pub(crate) text: Option<String>,
    pub(crate) usage: Option<Usage>,
    pub(crate) error: Option<String>,
    pub(crate) result_message_id: Option<String>,
    pub(crate) final_message: Option<Message>,
}
