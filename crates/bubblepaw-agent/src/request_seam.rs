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

/// Required methods are what the loop relies on to build a request and read the
/// history back. Defaulted methods are hooks for a host that keeps more than the
/// transcript; the defaults are right for one that keeps nothing else.
#[async_trait]
pub trait RequestContext: Send + Sync {
    /// Required: the session's history for a request, read from where the last one stopped.
    fn history(&self, session: &str) -> Result<Vec<Message>>;
    /// Required: the provider extension history cursor for the messages a request
    /// sends, remembering them for the next one.
    fn history_cursor(&self, session: &str, messages: &[Message]) -> HistoryCursor;
    /// Required: the messages of a draft request: the history as the host projects it,
    /// the steering batch intact after it, then the preview of the notes about to go.
    fn draft(&self, session: &str, history: &[Message], steering: &[Message]) -> Result<Draft>;
    /// Required: the host's preparation of a draft request. It may wait on auxiliary work.
    async fn prepare(
        &self,
        session: &str,
        request: ModelRequest,
        provider: Arc<dyn Provider>,
        mode: PrepareMode<'_>,
        cancellation: CancellationToken,
    ) -> Result<PreparedContext>;
    /// Required: the final request, with pending notes appended and the host's budget
    /// checked. A request the budget refuses comes back as `TooLarge`.
    fn build(
        &self,
        session: &str,
        request: ModelRequest,
        scope: Option<&ObservationScope>,
        provider: Arc<dyn Provider>,
    ) -> Result<Built>;
    /// Required: the provider as the run's main requests are made through it. A host
    /// that meters or wraps requests returns its wrapper; otherwise the provider itself.
    fn requester(&self, run: &Run, provider: Arc<dyn Provider>) -> Arc<dyn Provider>;
    /// Required: a note for the model, queued for the next request. A host that
    /// delivers no notes drops it, and the model never sees tool reminders.
    fn note(&self, session: &str, note: Note<'_>);
    /// Override when the host keeps an applied context view ahead of the history.
    /// The default is no such messages, which is right when the history is the whole view.
    fn context_prefix(&self, _session: &str) -> Result<Vec<Message>> {
        Ok(Vec::new())
    }
    /// Override with `note` and `draft`, which must agree on the pending preview. The
    /// default is no pending note, which is right when `draft` returns no preview.
    fn reminder_preview(&self, _session: &str) -> Result<Option<Message>> {
        Ok(None)
    }
    /// Override to hold a host resource for the run's turn; it is let go when the
    /// returned value drops. The default holds nothing.
    fn turn_scope(&self, _run: &Run) -> Box<dyn std::any::Any + Send> {
        Box::new(())
    }
    /// Override to observe or meter a model request that is about to start. The default
    /// ignores it, which is right when nothing is recorded.
    fn model_started(
        &self,
        _run: &Run,
        _scope: Option<&ObservationScope>,
        _request: &ModelRequest,
        _provider: &Arc<dyn Provider>,
    ) {
    }
    /// Override to record the final request just before it is dispatched. The default
    /// ignores it.
    fn request_sent(&self, _session: &str, _request: &ModelRequest, _provider: Arc<dyn Provider>) {}
    /// Override to settle what the request recorded when it ends with this result. The
    /// default leaves the result as it is.
    fn model_finished(
        &self,
        _run: &Run,
        _scope: Option<&ObservationScope>,
        _result: &mut Result<ModelResponse>,
        _timing: (u64, Option<u64>),
        _cancelled: bool,
    ) {
    }
    /// Override to record a reply the run accepted. The default ignores it.
    fn response_received(&self, _session: &str, _response: &ModelResponse) {}
}
