//! What the run loop is built from and what one run hands it, and the shell's
//! builders for both. The loop holds seams and plain values only.
use super::tool_source::ToolSource;
use super::*;
use crate::live_events::LiveEvents;

/// The plain config values the loop reads.
#[derive(Clone)]
pub(super) struct LoopLimits {
    pub(super) turn_start: Option<Arc<dyn TurnStart>>,
    /// The default round limit; a run's own option overrides it.
    pub(super) max_rounds: Option<usize>,
    pub(super) max_request_bytes: usize,
    pub(super) max_paragraph_bytes: usize,
}

/// The loop for one session shell: the seams it writes and asks through, and its limits.
pub(super) struct RunLoop {
    pub(super) journal: Arc<dyn Journal>,
    pub(super) live: Arc<dyn LiveEvents>,
    pub(super) request_context: Arc<dyn RequestContext>,
    /// The default model and the request provider, asked afresh each round.
    pub(super) provider: ProviderResolver,
    pub(super) observation: Option<Observation>,
    pub(super) limits: LoopLimits,
}

/// Builds the dispatcher a run starts with, over its first tools.
pub(super) type NewDispatcher = dyn Fn(ToolRegistry) -> ToolDispatcher + Send + Sync;

/// What one run needs to start: the run, its options and how it is stopped.
#[derive(Clone, Copy)]
pub(super) struct RunInputs<'a> {
    pub(super) run: &'a Run,
    pub(super) options: &'a RunOptions,
    pub(super) cancellation: &'a CancellationToken,
    pub(super) single_tool: Option<&'a SingleToolRun>,
    /// The tools each round offers.
    pub(super) tools: &'a dyn ToolSource,
    /// A fully configured dispatcher over the tools a run starts with, built once per run.
    pub(super) dispatcher: &'a NewDispatcher,
    /// Whether the gate is told the user's latest request, kept current as steering arrives.
    pub(super) wants_user_request: bool,
}

/// How a run ended, for the shell to record on the run and commit.
pub(super) struct RunOutcome {
    pub(super) status: String,
    pub(super) text: Option<String>,
    pub(super) usage: Option<Usage>,
    pub(super) error: Option<String>,
    pub(super) result_message_id: Option<String>,
    pub(super) final_message: Option<Message>,
}

impl Harness {
    /// The loop over this harness's journal, live sink, request context, provider
    /// binding and limits.
    pub(super) fn run_loop(&self) -> RunLoop {
        let provider = self.config.provider_resolver.clone().unwrap_or_else(|| {
            let (model, provider) = (self.config.model.clone(), self.provider.clone());
            Arc::new(move || (model.clone(), provider.clone()))
        });
        RunLoop {
            journal: self.journal.clone(),
            live: self.live.clone(),
            request_context: self.request_context.clone(),
            provider,
            observation: self.config.observation.clone(),
            limits: LoopLimits {
                turn_start: self.config.turn_start.clone(),
                max_rounds: self.config.max_rounds,
                max_request_bytes: self.config.max_request_bytes,
                max_paragraph_bytes: self.config.max_paragraph_bytes,
            },
        }
    }
    /// The dispatcher a run starts with over its first tools: the config's approval
    /// policy, the run's override, its tool budget and whether the user may be asked.
    pub(super) fn run_dispatcher(
        &self,
        options: &RunOptions,
        single_tool: bool,
    ) -> Box<NewDispatcher> {
        let (journal, live, gate) = (self.journal.clone(), self.live.clone(), self.stock_gate());
        let observation = self.config.observation.clone();
        let max_result_bytes = self.config.max_tool_result_bytes;
        let setup = crate::ApprovalPolicy::context_setup(
            self.config.approval.clone(),
            self.config
                .run_approval_resolver(options.approval.clone(), single_tool),
        );
        let budget = options.max_tool_calls;
        Box::new(move |tools| {
            ToolDispatcher::from_seams(
                tools,
                max_result_bytes,
                observation.clone(),
                journal.clone(),
                gate.clone(),
                live.clone(),
                setup.clone(),
            )
            .with_tool_budget(budget)
            .with_interactive(!single_tool)
        })
    }
}
