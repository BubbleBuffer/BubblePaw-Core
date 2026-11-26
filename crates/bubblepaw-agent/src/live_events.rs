//! How the run loop and the dispatcher show what is happening right now. Live
//! events reach subscribers only: they are never stored, and the durable events
//! stay with the journal.
use crate::tool_result::ToolResult;
use bubblepaw_ai::ToolCall;

/// What the loop and the dispatcher show live.
pub enum LiveEvent {
    /// One finished paragraph of the reply.
    Paragraph {
        text: String,
    },
    /// A model request is about to start, including correction retries.
    ModelRequestStarted,
    /// One text batch as the provider streams it.
    TextDelta {
        text: String,
    },
    /// One batch of the model's readable thinking.
    ThinkingDelta {
        text: String,
    },
    /// The thinking streamed so far belongs to an attempt that was retried or fell back.
    ThinkingDiscarded,
    ToolStarted {
        call: ToolCall,
    },
    ToolFinished {
        call_id: String,
        name: String,
        result: ToolResult,
    },
}

pub trait LiveEvents: Send + Sync {
    /// A live event outside the streamed text, such as a tool starting or finishing.
    fn transient(&self, session_id: &str, run_id: &str, event: LiveEvent);
    /// One streamed text or thinking batch.
    fn progress(&self, session_id: &str, run_id: &str, event: LiveEvent);
    /// One finished paragraph of streamed text.
    fn paragraph(&self, session_id: &str, run_id: &str, text: String);
}
