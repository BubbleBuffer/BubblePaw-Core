//! The listener the loop hands a provider for one model request: it turns the
//! adapter's stream events into live host events.
use crate::{
    CancellationToken, EventKind, StreamCallback, StreamEvent, event_log::Log, stream::Paragraphs,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

/// Everything the listener keeps. It holds the event log, never the store or
/// engine, so a provider that retains it retains neither.
pub(super) struct ModelStream {
    pub(super) log: Arc<Log>,
    pub(super) session: String,
    pub(super) run_id: String,
    pub(super) live: Arc<AtomicBool>,
    pub(super) cancellation: CancellationToken,
    pub(super) paragraphs: Arc<Mutex<Paragraphs>>,
    pub(super) max_paragraph_bytes: usize,
    pub(super) started: Instant,
    pub(super) first_text: Option<Arc<Mutex<Option<u64>>>>,
}

impl ModelStream {
    /// Text deltas also feed the paragraph buffer; thinking deltas are live
    /// progress only, in the order the provider streams them.
    pub(super) fn callback(self) -> StreamCallback {
        Arc::new(move |event| {
            if !self.live.load(Ordering::Relaxed) || self.cancellation.is_cancelled() {
                return;
            }
            let text = match event {
                StreamEvent::ThinkingDelta(text) => {
                    if !text.is_empty() {
                        let kind = EventKind::ThinkingDelta {
                            text: text.to_owned(),
                        };
                        self.log.progress(&self.session, &self.run_id, kind);
                    }
                    return;
                }
                StreamEvent::TextDelta(text) => text,
            };
            if !text.is_empty() {
                if let Some(first) = &self.first_text {
                    first
                        .lock()
                        .unwrap()
                        .get_or_insert(self.started.elapsed().as_millis() as u64);
                }
                let kind = EventKind::TextDelta {
                    text: text.to_owned(),
                };
                self.log.progress(&self.session, &self.run_id, kind);
            }
            let mut buffer = self.paragraphs.lock().unwrap();
            for piece in buffer.push_bounded(text, self.max_paragraph_bytes) {
                self.log.paragraph(&self.session, &self.run_id, piece);
            }
        })
    }
}
