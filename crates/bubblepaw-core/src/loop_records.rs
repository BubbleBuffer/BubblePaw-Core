//! What the loop holds and returns for one run: the outcome of a turn, the single
//! tool call a run may consist of, and the lifetime of the stream callback.
use crate::run_data::Run;
use crate::tool_result::ToolResult;
use anyhow::Result;
use bubblepaw_ai::{Message, MessageOrigin, Role, ToolCall};
use serde_json::Value;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

pub(crate) struct CallbackLifetime(pub(crate) Arc<AtomicBool>);
pub(crate) struct TurnOutcome {
    pub(crate) text: String,
    pub(crate) result_message_id: String,
    pub(crate) final_message: Option<Message>,
}
/// A run that is one tool call: the call, and the slot its result is left in.
pub(crate) struct SingleToolRun {
    pub(crate) call: ToolCall,
    pub(crate) result: Arc<Mutex<Option<ToolResult>>>,
}
impl Drop for CallbackLifetime {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

pub(crate) fn outcome(run: &Run, value: Value) -> Result<TurnOutcome> {
    let text = serde_json::to_string(&value)?;
    let mut final_message = Message::new(&run.session_id, Role::Assistant, text.clone());
    final_message.origin = Some(MessageOrigin::Generated);
    Ok(TurnOutcome {
        text,
        result_message_id: final_message.id.clone(),
        final_message: Some(final_message),
    })
}

/// A tool result asked to end the run: the run completes with that tool message.
pub(crate) fn terminated(message: &Message) -> TurnOutcome {
    TurnOutcome {
        text: message.text().into_owned(),
        result_message_id: message.id.clone(),
        final_message: None,
    }
}

pub(crate) fn text_outcome(message: Message, needs_commit: bool) -> TurnOutcome {
    TurnOutcome {
        text: message.text().into_owned(),
        result_message_id: message.id.clone(),
        final_message: needs_commit.then_some(message),
    }
}
