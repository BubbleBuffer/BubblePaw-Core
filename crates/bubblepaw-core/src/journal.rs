//! The run loop writes things down through this trait before going on. Each
//! method is one atomic unit named by what happened in the run; the durable
//! events that belong to it are emitted by the implementation, in the same commit.
use crate::{ClientInfo, Message, OutputRecord, QueuedInput, Run, ToolCall};
use crate::{ToolResult, journal_admission::RunAdmission};
use anyhow::Result;

/// Which pending inputs a query asks about.
#[derive(Clone, Copy)]
pub(crate) enum InputScope {
    Any,
    Steering,
}

/// Which pending inputs a consumption takes.
#[derive(Clone, Copy)]
pub(crate) enum Consumed {
    Steering,
    GeneratedSteering,
}

/// A run's final state, with the reply that settles it.
pub(crate) struct RunFinish<'a> {
    pub run: &'a Run,
    pub result_message_id: Option<&'a str>,
    pub final_message: Option<&'a Message>,
    pub origin: Option<ClientInfo>,
    /// The session settles with the run; false when a continuation follows.
    pub settles_session: bool,
}

pub(crate) trait Journal: Send + Sync {
    /// Input accepted and the run admitted: run row, input, RunStarted.
    fn run_admitted(&self, admission: RunAdmission<'_>) -> Result<()>;
    /// Steering or follow-up input queued; announced only while a run is active.
    fn input_queued(
        &self,
        input: &QueuedInput,
        run_id: Option<&str>,
        client: Option<ClientInfo>,
    ) -> Result<()>;
    /// Whether input of this scope is pending.
    fn input_pending(&self, session_id: &str, scope: InputScope) -> Result<bool>;
    /// Pending input of this kind consumed; the number consumed.
    fn inputs_consumed(&self, session_id: &str, which: Consumed) -> Result<usize>;
    /// The pending steering batch consumed into messages, in order.
    fn steering_batch_consumed(&self, session_id: &str) -> Result<Vec<Message>>;
    /// Whether a tool call id is already recorded for the session.
    fn tool_call_recorded(&self, session_id: &str, call_id: &str) -> Result<bool>;
    /// A reply with no tool dispatch to follow in the same commit. The request
    /// usage the run is waiting to write goes with it.
    fn reply_received(&self, run_id: &str, message: &Message) -> Result<()>;
    /// A reply and the dispatch of its one call: message, usage, effect, ToolStarted.
    /// Returns the invocation id.
    fn reply_dispatched(
        &self,
        session_id: &str,
        run_id: &str,
        message: &Message,
        call: &ToolCall,
    ) -> Result<String>;
    /// A call started: its effect (when dispatched) and ToolStarted.
    fn tool_started(
        &self,
        session_id: &str,
        run_id: &str,
        message_id: &str,
        call: &ToolCall,
        dispatched: bool,
    ) -> Result<Option<String>>;
    /// A call finished: the tool message, its effect state and ToolFinished.
    fn tool_finished(
        &self,
        run_id: &str,
        message: &Message,
        call: &ToolCall,
        result: ToolResult,
        invocation: Option<&str>,
    ) -> Result<()>;
    /// A nested call of a session dispatched: its effect. Returns the invocation id.
    fn nested_dispatched(&self, session_id: &str, run_id: &str, call: &ToolCall) -> Result<String>;
    /// A call's outcome became unknown: its effect, when still dispatched.
    fn effect_unknown(&self, session_id: &str, invocation: &str) -> Result<()>;
    /// A background effect completed, wherever its session is stored.
    fn effect_completed(&self, invocation: &str) -> Result<()>;
    /// An effect of a known session completed, without restoring archives.
    fn effect_completed_for_session(&self, session_id: &str, invocation: &str) -> Result<()>;
    /// An output record and its event.
    fn tool_output(&self, record: &OutputRecord) -> Result<()>;
    /// The run ended: reply, usage, run row, RunFinished and maybe SessionSettled.
    fn run_finished(&self, finish: RunFinish<'_>) -> Result<()>;
    /// A queued follow-up answered by a new run: run row, input, RunStarted.
    fn run_continued(&self, next: &Run) -> Result<()>;
    /// The session settled without a continuation having started.
    fn session_settled(&self, session_id: &str, run_id: &str);
    /// What streamed before a failed or cancelled request, kept as a reply.
    fn partial_reply_kept(&self, message: &Message) -> Result<()>;
    /// Effects of a run that ended unresolved, closed.
    fn run_closed(&self, session_id: &str, run_id: &str) -> Result<()>;
}
