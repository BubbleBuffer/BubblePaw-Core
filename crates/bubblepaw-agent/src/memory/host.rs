//! Assembles the in-memory seams into a loop that runs one prompt per call.
use super::context::MemoryContext;
use super::journal::MemoryJournal;
use super::seams::{AllowAll, CollectedEvents, FixedTools};
use crate::dispatch::ToolDispatcher;
use crate::ids::{id, now};
use crate::live_events::LiveEvents;
use crate::loop_options::LoopOptions;
use crate::run_data::Run;
use crate::run_inputs::{LoopLimits, RunInputs, RunLoop, RunOutcome};
use crate::run_journal::RunJournal;
use crate::tool_gate::ToolGate;
use crate::tool_registry::ToolRegistry;
use bubblepaw_ai::{CancellationToken, Message, MessageOrigin, Provider, Role};
use std::sync::Arc;

const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
const MAX_TOOL_RESULT_BYTES: usize = 64 * 1024;
const MAX_PARAGRAPH_BYTES: usize = 16 * 1024;

/// One conversation in memory. Each [`MemoryHost::run`] adds a user message to the
/// shared transcript, runs the loop to its outcome and writes the final reply.
pub struct MemoryHost {
    /// The transcript and recorded events of every run so far.
    pub journal: Arc<MemoryJournal>,
    /// The live events of every run so far, until taken.
    pub events: Arc<CollectedEvents>,
    session_id: String,
    run_loop: RunLoop,
    tools: FixedTools,
    gate: Arc<dyn ToolGate>,
}

impl MemoryHost {
    /// A host over `provider`, asking for `model`, offering `tools` under `system_prompt`.
    pub fn new(
        model: impl Into<String>,
        provider: Arc<dyn Provider>,
        tools: ToolRegistry,
        system_prompt: impl Into<String>,
    ) -> Self {
        let journal = Arc::new(MemoryJournal::new());
        let events = Arc::new(CollectedEvents::new());
        let model = model.into();
        let run_loop = RunLoop {
            journal: journal.clone(),
            live: events.clone(),
            request_context: Arc::new(MemoryContext::new(journal.clone())),
            provider: Arc::new(move || (model.clone(), provider.clone())),
            observation: None,
            limits: LoopLimits {
                turn_start: None,
                max_rounds: None,
                max_request_bytes: MAX_REQUEST_BYTES,
                max_paragraph_bytes: MAX_PARAGRAPH_BYTES,
            },
        };
        Self {
            journal,
            events,
            session_id: id(),
            run_loop,
            tools: FixedTools::new(tools, system_prompt),
            gate: Arc::new(AllowAll),
        }
    }
    /// Decide each call with `gate` instead of allowing all of them.
    pub fn with_gate(mut self, gate: Arc<dyn ToolGate>) -> Self {
        self.gate = gate;
        self
    }
    /// The session the transcript belongs to.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    /// Run `prompt` as a user message. The outcome is the loop's; its final reply,
    /// when it has one, is already in the journal.
    pub async fn run(
        &self,
        prompt: &str,
        options: &LoopOptions,
        cancellation: &CancellationToken,
    ) -> RunOutcome {
        let mut user = Message::new(&self.session_id, Role::User, prompt.into());
        user.origin = Some(MessageOrigin::Typed);
        self.journal.push(user);
        let run = Run {
            id: id(),
            session_id: self.session_id.clone(),
            model: options.model.clone().unwrap_or_default(),
            status: "running".into(),
            text: None,
            error: None,
            started_at: now(),
            ended_at: None,
            usage: None,
        };
        let journal = self.journal.clone() as Arc<dyn RunJournal>;
        let live = self.events.clone() as Arc<dyn LiveEvents>;
        let gate = self.gate.clone();
        let budget = options.max_tool_calls;
        let dispatcher = move |tools| {
            ToolDispatcher::from_seams(
                tools,
                MAX_TOOL_RESULT_BYTES,
                None,
                journal.clone(),
                gate.clone(),
                live.clone(),
                Arc::new(|_| {}),
            )
            .with_tool_budget(budget)
        };
        let outcome = self
            .run_loop
            .run_once(&RunInputs {
                run: &run,
                options,
                cancellation,
                single_tool: None,
                tools: &self.tools,
                dispatcher: &dispatcher,
                wants_user_request: false,
            })
            .await;
        if let Some(message) = &outcome.final_message {
            self.journal.push(message.clone());
        }
        outcome
    }
}
