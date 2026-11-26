//! What the run loop and the tool dispatcher write down, and ask, through the
//! journal. Each method is one atomic unit named by what happened in the run; the
//! durable events that belong to it are emitted by the implementation, in the same
//! commit. The shell's own writes (admission, finish) are the session journal.
use crate::run_data::OutputRecord;
use crate::tool_result::ToolResult;
use anyhow::Result;
use bubblepaw_ai::{Message, ToolCall};

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

pub(crate) trait RunJournal: Send + Sync {
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
    /// What streamed before a failed or cancelled request, kept as a reply.
    fn partial_reply_kept(&self, message: &Message) -> Result<()>;
    /// Effects of a run that ended unresolved, closed.
    fn run_closed(&self, session_id: &str, run_id: &str) -> Result<()>;
}
