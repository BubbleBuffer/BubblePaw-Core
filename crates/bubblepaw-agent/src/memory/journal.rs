//! A run journal that keeps what the run writes in a mutex.
use crate::ids::id;
use crate::run_data::OutputRecord;
use crate::run_journal::{Consumed, InputScope, RunJournal};
use crate::tool_result::ToolResult;
use anyhow::Result;
use bubblepaw_ai::{Message, Role, ToolCall};
use std::sync::{Mutex, MutexGuard};

/// A durable event the journal recorded, in the order it happened.
#[derive(Debug, Clone)]
pub enum JournalEvent {
    ToolStarted {
        call: ToolCall,
    },
    ToolFinished {
        call_id: String,
        name: String,
        result: ToolResult,
    },
}

#[derive(Default)]
struct State {
    transcript: Vec<Message>,
    steering: Vec<Message>,
    events: Vec<JournalEvent>,
    outputs: Vec<OutputRecord>,
    closed: bool,
}

/// The transcript, queued steering and recorded events of the runs it served.
/// The request context reads its history from here.
#[derive(Default)]
pub struct MemoryJournal {
    state: Mutex<State>,
}

impl MemoryJournal {
    pub fn new() -> Self {
        Self::default()
    }
    /// A journal that starts with this history.
    pub fn with_history(history: Vec<Message>) -> Self {
        let journal = Self::new();
        journal.state().transcript = history;
        journal
    }
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }
    /// Append a message the caller writes: a run's user message or its final reply.
    pub fn push(&self, message: Message) {
        self.state().transcript.push(message);
    }
    /// Queue a user message that steers the run in flight.
    pub fn queue_steering(&self, message: Message) {
        self.state().steering.push(message);
    }
    /// Every message written so far, in order.
    pub fn transcript(&self) -> Vec<Message> {
        self.state().transcript.clone()
    }
    /// The tool messages written so far, in order.
    pub fn tool_results(&self) -> Vec<Message> {
        let state = self.state();
        let tool = state.transcript.iter().filter(|m| m.role == Role::Tool);
        tool.cloned().collect()
    }
    /// The tool events recorded so far, in order.
    pub fn events(&self) -> Vec<JournalEvent> {
        self.state().events.clone()
    }
    /// The output records written so far, in order.
    pub fn outputs(&self) -> Vec<OutputRecord> {
        self.state().outputs.clone()
    }
    /// Whether a run ended unresolved and was closed.
    pub fn is_closed(&self) -> bool {
        self.state().closed
    }
    fn take_steering(&self) -> Vec<Message> {
        let mut state = self.state();
        let batch = std::mem::take(&mut state.steering);
        state.transcript.extend(batch.iter().cloned());
        batch
    }
}

impl RunJournal for MemoryJournal {
    fn input_pending(&self, _session_id: &str, _scope: InputScope) -> Result<bool> {
        Ok(!self.state().steering.is_empty())
    }
    fn inputs_consumed(&self, _session_id: &str, which: Consumed) -> Result<usize> {
        Ok(match which {
            Consumed::Steering => self.take_steering().len(),
            // No input here is host-generated.
            Consumed::GeneratedSteering => 0,
        })
    }
    fn steering_batch_consumed(&self, _session_id: &str) -> Result<Vec<Message>> {
        Ok(self.take_steering())
    }
    fn tool_call_recorded(&self, _session_id: &str, call_id: &str) -> Result<bool> {
        let state = self.state();
        let recorded = state
            .transcript
            .iter()
            .flat_map(|message| &message.tool_calls)
            .any(|call| call.id == call_id);
        Ok(recorded)
    }
    fn reply_received(&self, _run_id: &str, message: &Message) -> Result<()> {
        self.push(message.clone());
        Ok(())
    }
    fn reply_dispatched(
        &self,
        _session_id: &str,
        _run_id: &str,
        message: &Message,
        call: &ToolCall,
    ) -> Result<String> {
        let mut state = self.state();
        state.transcript.push(message.clone());
        state
            .events
            .push(JournalEvent::ToolStarted { call: call.clone() });
        Ok(id())
    }
    fn tool_started(
        &self,
        _session_id: &str,
        _run_id: &str,
        _message_id: &str,
        call: &ToolCall,
        dispatched: bool,
    ) -> Result<Option<String>> {
        self.state()
            .events
            .push(JournalEvent::ToolStarted { call: call.clone() });
        Ok(dispatched.then(id))
    }
    fn tool_finished(
        &self,
        _run_id: &str,
        message: &Message,
        call: &ToolCall,
        result: ToolResult,
        _invocation: Option<&str>,
    ) -> Result<()> {
        let mut state = self.state();
        state.transcript.push(message.clone());
        state.events.push(JournalEvent::ToolFinished {
            call_id: call.id.clone(),
            name: call.name.clone(),
            result,
        });
        Ok(())
    }
    fn tool_output(&self, record: &OutputRecord) -> Result<()> {
        self.state().outputs.push(record.clone());
        Ok(())
    }
    fn partial_reply_kept(&self, message: &Message) -> Result<()> {
        self.push(message.clone());
        Ok(())
    }
    fn run_closed(&self, _session_id: &str, _run_id: &str) -> Result<()> {
        self.state().closed = true;
        Ok(())
    }
}
