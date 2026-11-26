//! What the run loop asks about the request it sends to the model. Each method is
//! one question named by the loop's need; the implementation owns the history it
//! keeps, the notes it queues and the records and hook calls that answering needs.
//! The loop decides when to ask and what to do with the answer.
use crate::observer::ObservationScope;
use crate::prepare_types::{PrepareMode, PreparedContext};
use crate::reminder_types::Reminder;
use crate::run_data::Run;
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_ai::{
    CancellationToken, HistoryCursor, Message, ModelRequest, ModelResponse, Provider,
};
use std::sync::Arc;

/// A note for the model, queued for the next request.
pub enum Note<'a> {
    /// Sent when its text differs from the last copy of the same kind.
    Text { kind: &'a str, text: &'a str },
    /// Sent once even when its text is unchanged: the last copy left the model's view.
    Fresh { kind: &'a str, text: &'a str },
    /// A note a tool queued, with its delivery identity.
    Reminder(Reminder),
    /// Staged for the end of the cold tier of the next request.
    Cold { kind: &'a str, text: &'a str },
}

/// The messages of a draft request, and the note preview they end with.
pub struct Draft {
    pub messages: Vec<Message>,
    pub preview: Option<Message>,
}

/// A final request the host's budget refused, and how to ask again.
pub struct TooLarge {
    /// The host's own diagnostic, to return when asking again would not help.
    pub error: anyhow::Error,
    pub additional_tokens: usize,
    pub additional_bytes: usize,
    /// How far the host's view covers history now.
    pub frontier: Option<i64>,
}

pub enum Built {
    Ready(Box<ModelRequest>),
    TooLarge(TooLarge),
}

/// How much a session's request state is let go.
#[derive(Clone, Copy)]
pub enum Release {
    /// The session is gone for good.
    Deleted,
    /// The session is idle; its history is read again on the next request.
    Idle,
}

#[async_trait]
pub trait RequestContext: Send + Sync {
    /// The session's history for a request, read from where the last one stopped.
    fn history(&self, session: &str) -> Result<Vec<Message>>;
    /// The provider extension history cursor for the messages a request sends,
    /// remembering them for the next one.
    fn history_cursor(&self, session: &str, messages: &[Message]) -> HistoryCursor;
    /// The messages of a draft request: the history as the host projects it, the
    /// steering batch intact after it, then the preview of the notes about to go.
    fn draft(&self, session: &str, history: &[Message], steering: &[Message]) -> Result<Draft>;
    /// The messages the host's applied context view keeps ahead of the history,
    /// oldest first. Empty when the session has none.
    fn context_prefix(&self, session: &str) -> Result<Vec<Message>>;
    /// The message the pending notes would make, without sending them.
    fn reminder_preview(&self, session: &str) -> Result<Option<Message>>;
    /// The host's preparation of a draft request. It may wait on auxiliary work.
    async fn prepare(
        &self,
        session: &str,
        request: ModelRequest,
        provider: Arc<dyn Provider>,
        mode: PrepareMode<'_>,
        cancellation: CancellationToken,
    ) -> Result<PreparedContext>;
    /// The final request, with pending notes appended and the host's budget checked.
    /// A request the budget refuses comes back as `TooLarge`.
    fn build(
        &self,
        session: &str,
        request: ModelRequest,
        scope: Option<&ObservationScope>,
        provider: Arc<dyn Provider>,
    ) -> Result<Built>;
    /// A note for the model, queued for the next request.
    fn note(&self, session: &str, note: Note<'_>);
    /// The serving prefix is about to change, for this reason.
    fn declare_change(&self, session: &str, reason: &str);
    /// The reason the host deliberately changed the serving prefix since the last
    /// call, taken once and declared.
    fn take_prefix_change(&self, session: &str) -> Option<String>;
    /// Whether the tool inventory generation differs from the one seen last.
    fn tool_generation_observed(&self, session: &str, generation: u64) -> bool;
    /// What the host keeps for a run is let go when this drops, however the run ends.
    fn run_hold(&self, run: &Run) -> Box<dyn std::any::Any + Send>;
    /// What the host holds for the run's turn is let go when this drops, however the turn ends.
    fn turn_scope(&self, run: &Run) -> Box<dyn std::any::Any + Send>;
    /// The provider as the run's main requests are made through it.
    fn requester(&self, run: &Run, provider: Arc<dyn Provider>) -> Arc<dyn Provider>;
    /// A model request is about to start.
    fn model_started(
        &self,
        run: &Run,
        scope: Option<&ObservationScope>,
        request: &ModelRequest,
        provider: &Arc<dyn Provider>,
    );
    /// The final request, immediately before it is dispatched.
    fn request_sent(&self, session: &str, request: &ModelRequest, provider: Arc<dyn Provider>);
    /// A model request ended with this result.
    fn model_finished(
        &self,
        run: &Run,
        scope: Option<&ObservationScope>,
        result: &mut Result<ModelResponse>,
        timing: (u64, Option<u64>),
        cancelled: bool,
    );
    /// A reply was accepted for the run.
    fn response_received(&self, session: &str, response: &ModelResponse);
    /// The session's request state is let go.
    fn release(&self, session: &str, how: Release);
}
