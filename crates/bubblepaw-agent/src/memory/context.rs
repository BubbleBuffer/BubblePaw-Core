//! A request context that builds each request from the journal's transcript.
use super::journal::MemoryJournal;
use crate::observer::ObservationScope;
use crate::prepare_types::{PrepareMode, PreparedContext};
use crate::request_seam::{Built, Draft, Note, RequestContext};
use crate::run_data::Run;
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_ai::{
    CancellationToken, HistoryCursor, HistoryTracker, Message, ModelRequest, Provider,
};
use std::sync::Arc;

/// The history is the journal's transcript, sent whole: no compaction, no size
/// budget, no prefix tracking. Notes that tools queue for the model are dropped.
pub struct MemoryContext {
    journal: Arc<MemoryJournal>,
    cursors: HistoryTracker,
}

impl MemoryContext {
    pub fn new(journal: Arc<MemoryJournal>) -> Self {
        Self {
            journal,
            cursors: HistoryTracker::default(),
        }
    }
    /// Let go of a session's history cursor.
    pub fn forget_session(&self, session: &str) {
        self.cursors.forget(session);
    }
}

#[async_trait]
impl RequestContext for MemoryContext {
    fn history(&self, _session: &str) -> Result<Vec<Message>> {
        Ok(self.journal.transcript())
    }
    fn history_cursor(&self, session: &str, messages: &[Message]) -> HistoryCursor {
        self.cursors.advance(session, messages)
    }
    fn draft(&self, _session: &str, history: &[Message], steering: &[Message]) -> Result<Draft> {
        let mut messages: Vec<Message> = history
            .iter()
            .filter(|saved| !steering.iter().any(|steer| steer.id == saved.id))
            .cloned()
            .collect();
        messages.extend(steering.iter().cloned());
        Ok(Draft {
            messages,
            preview: None,
        })
    }
    async fn prepare(
        &self,
        _session: &str,
        request: ModelRequest,
        _provider: Arc<dyn Provider>,
        _mode: PrepareMode<'_>,
        _cancellation: CancellationToken,
    ) -> Result<PreparedContext> {
        Ok(PreparedContext {
            request,
            notice: None,
            usage: None,
        })
    }
    fn build(
        &self,
        _session: &str,
        request: ModelRequest,
        _scope: Option<&ObservationScope>,
        _provider: Arc<dyn Provider>,
    ) -> Result<Built> {
        Ok(Built::Ready(Box::new(request)))
    }
    fn note(&self, _session: &str, _note: Note<'_>) {}
    fn requester(&self, _run: &Run, provider: Arc<dyn Provider>) -> Arc<dyn Provider> {
        provider
    }
}
