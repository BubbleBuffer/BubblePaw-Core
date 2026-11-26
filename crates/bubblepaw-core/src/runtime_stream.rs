//! The listener the loop hands a provider for one model request: it turns the
//! adapter's stream events into live host events.
use crate::live_events::{LiveEvent, LiveEvents};
use crate::stream::Paragraphs;
use bubblepaw_ai::{
    CancellationToken, IncompleteReason, Message, ModelResponse, Role, StreamCallback, StreamEvent,
    reply_blocks,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

/// Everything the listener keeps. It holds the live-event sink, never the store or
/// engine, so a provider that retains it retains neither.
pub(crate) struct ModelStream {
    pub(crate) events: Arc<dyn LiveEvents>,
    pub(crate) session: String,
    pub(crate) run_id: String,
    pub(crate) live: Arc<AtomicBool>,
    pub(crate) cancellation: CancellationToken,
    pub(crate) paragraphs: Arc<Mutex<Paragraphs>>,
    pub(crate) max_paragraph_bytes: usize,
    pub(crate) started: Instant,
    pub(crate) first_text: Option<Arc<Mutex<Option<u64>>>>,
    pub(crate) partial: SharedPartial,
}

/// What the current model request has streamed so far: the text, and the
/// thinking that was not retracted. A request that fails or is cancelled keeps
/// it as an incomplete assistant message.
#[derive(Default)]
pub(crate) struct PartialReply {
    thinking: String,
    text: String,
}
pub(crate) type SharedPartial = Arc<Mutex<PartialReply>>;

impl PartialReply {
    pub(crate) fn clear(&mut self) {
        self.thinking.clear();
        self.text.clear();
    }

    /// The incomplete assistant message, unless nothing but whitespace or
    /// thinking streamed.
    pub(crate) fn message(
        &self,
        session_id: &str,
        reason: IncompleteReason,
        limit: usize,
    ) -> Option<Message> {
        if self.text.trim().is_empty() {
            return None;
        }
        // A reply this large could not have been stored; drop it whole.
        let reply = ModelResponse {
            text: self.text.clone(),
            thinking: self.thinking.clone(),
            ..Default::default()
        };
        if reply.storage_bytes().map_or(true, |bytes| bytes > limit) {
            return None;
        }
        let mut message = Message::new(session_id, Role::Assistant, self.text.clone());
        message.content = reply_blocks(&self.thinking, &self.text);
        message.incomplete = Some(reason);
        Some(message)
    }
}

impl ModelStream {
    /// Text deltas also feed the paragraph buffer; thinking deltas are live
    /// progress only, in the order the provider streams them.
    pub(crate) fn callback(self) -> StreamCallback {
        Arc::new(move |event| {
            if !self.live.load(Ordering::Relaxed) || self.cancellation.is_cancelled() {
                return;
            }
            let text = match event {
                StreamEvent::ThinkingDelta(text) => {
                    if !text.is_empty() {
                        self.partial.lock().unwrap().thinking.push_str(text);
                        let event = LiveEvent::ThinkingDelta {
                            text: text.to_owned(),
                        };
                        self.events.progress(&self.session, &self.run_id, event);
                    }
                    return;
                }
                StreamEvent::ThinkingDiscarded => {
                    self.partial.lock().unwrap().thinking.clear();
                    let event = LiveEvent::ThinkingDiscarded;
                    self.events.progress(&self.session, &self.run_id, event);
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
                let event = LiveEvent::TextDelta {
                    text: text.to_owned(),
                };
                self.events.progress(&self.session, &self.run_id, event);
            }
            let mut buffer = self.paragraphs.lock().unwrap();
            for piece in buffer.push_bounded(text, self.max_paragraph_bytes) {
                self.events.paragraph(&self.session, &self.run_id, piece);
            }
        })
    }
}
