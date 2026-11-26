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
pub struct LoopLimits {
    pub turn_start: Option<Arc<dyn TurnStart>>,
    /// The default round limit; a run's own option overrides it.
    pub max_rounds: Option<usize>,
    pub max_request_bytes: usize,
    pub max_paragraph_bytes: usize,
}

/// The loop for one session shell: the seams it writes and asks through, and its limits.
pub struct RunLoop {
    pub journal: Arc<dyn RunJournal>,
    pub live: Arc<dyn LiveEvents>,
    pub request_context: Arc<dyn RequestContext>,
    /// The default model and the request provider, asked afresh each round.
    pub provider: ProviderResolver,
    pub observation: Option<Arc<dyn Observer>>,
    pub limits: LoopLimits,
}

/// Builds the dispatcher a run starts with, over its first tools.
pub type NewDispatcher = dyn Fn(ToolRegistry) -> ToolDispatcher + Send + Sync;

/// What one run needs to start: the run, its options and how it is stopped.
#[derive(Clone, Copy)]
pub struct RunInputs<'a> {
    pub run: &'a Run,
    pub options: &'a LoopOptions,
    pub cancellation: &'a CancellationToken,
    pub single_tool: Option<&'a SingleToolRun>,
    /// The tools each round offers.
    pub tools: &'a dyn ToolSource,
    /// A fully configured dispatcher over the tools a run starts with, built once per run.
    pub dispatcher: &'a NewDispatcher,
    /// Whether the gate is told the user's latest request, kept current as steering arrives.
    pub wants_user_request: bool,
}

/// How a run ended, for the shell to record on the run and commit.
pub struct RunOutcome {
    pub status: String,
    pub text: Option<String>,
    pub usage: Option<Usage>,
    pub error: Option<String>,
    pub result_message_id: Option<String>,
    /// The ordinary final reply, not yet written: the shell writes it when it ends the run.
    pub final_message: Option<Message>,
}
