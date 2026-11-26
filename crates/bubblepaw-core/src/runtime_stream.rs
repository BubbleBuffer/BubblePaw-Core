//! The listener the loop hands a provider for one model request: it turns the
//! adapter's stream events into live host events.
use crate::{
    CancellationToken, EventKind, Harness, IncompleteReason, Message, Role, StreamCallback,
    StreamEvent, live_events::LiveEvents, stream::Paragraphs,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

/// Everything the listener keeps. It holds the live-event sink, never the store or
/// engine, so a provider that retains it retains neither.
pub(super) struct ModelStream {
    pub(super) events: Arc<dyn LiveEvents>,
    pub(super) session: String,
    pub(super) run_id: String,
    pub(super) live: Arc<AtomicBool>,
    pub(super) cancellation: CancellationToken,
    pub(super) paragraphs: Arc<Mutex<Paragraphs>>,
    pub(super) max_paragraph_bytes: usize,
    pub(super) started: Instant,
    pub(super) first_text: Option<Arc<Mutex<Option<u64>>>>,
    pub(super) partial: SharedPartial,
}

/// What the current model request has streamed so far: the text, and the
/// thinking that was not retracted. A request that fails or is cancelled keeps
/// it as an incomplete assistant message.
#[derive(Default)]
pub(super) struct PartialReply {
    thinking: String,
    text: String,
}
pub(super) type SharedPartial = Arc<Mutex<PartialReply>>;

impl PartialReply {
    pub(super) fn clear(&mut self) {
        self.thinking.clear();
        self.text.clear();
    }

    /// The incomplete assistant message, unless nothing but whitespace or
    /// thinking streamed.
    fn message(&self, session_id: &str, reason: IncompleteReason, limit: usize) -> Option<Message> {
        if self.text.trim().is_empty() {
            return None;
        }
        // A reply this large could not have been stored; drop it whole.
        let reply = crate::ModelResponse {
            text: self.text.clone(),
            thinking: self.thinking.clone(),
            ..Default::default()
        };
        if reply.storage_bytes().map_or(true, |bytes| bytes > limit) {
            return None;
        }
        let mut message = Message::new(session_id, Role::Assistant, self.text.clone());
        message.content = crate::reply_blocks(&self.thinking, &self.text);
        message.incomplete = Some(reason);
        Some(message)
    }
}

impl Harness {
    /// Store what streamed before the request failed or was cancelled. The run's
    /// outcome stays as it was, so a failure to store is not reported.
    pub(super) fn keep_partial_reply(
        &self,
        session_id: &str,
        partial: &SharedPartial,
        cancelled: bool,
    ) {
        let reason = if cancelled {
            IncompleteReason::Cancelled
        } else {
            IncompleteReason::Error
        };
        let limit = self.config.max_request_bytes;
        let Some(message) = partial.lock().unwrap().message(session_id, reason, limit) else {
            return;
        };
        let _ = self.journal.partial_reply_kept(&message);
    }
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
                        self.partial.lock().unwrap().thinking.push_str(text);
                        let kind = EventKind::ThinkingDelta {
                            text: text.to_owned(),
                        };
                        self.events.progress(&self.session, &self.run_id, kind);
                    }
                    return;
                }
                StreamEvent::ThinkingDiscarded => {
                    self.partial.lock().unwrap().thinking.clear();
                    let kind = EventKind::ThinkingDiscarded {};
                    self.events.progress(&self.session, &self.run_id, kind);
                    return;
                }
                StreamEvent::TextDelta(text) => text,
                // The enum is non-exhaustive across crates; an unknown event is not reply text.
                _ => return,
            };
            if !text.is_empty() {
                self.partial.lock().unwrap().text.push_str(text);
                if let Some(first) = &self.first_text {
                    first
                        .lock()
                        .unwrap()
                        .get_or_insert(self.started.elapsed().as_millis() as u64);
                }
                let kind = EventKind::TextDelta {
                    text: text.to_owned(),
                };
                self.events.progress(&self.session, &self.run_id, kind);
            }
            let mut buffer = self.paragraphs.lock().unwrap();
            for piece in buffer.push_bounded(text, self.max_paragraph_bytes) {
                self.events.paragraph(&self.session, &self.run_id, piece);
            }
        })
    }
}
