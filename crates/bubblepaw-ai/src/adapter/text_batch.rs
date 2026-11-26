//! Batching of streamed text for the listener. Shared by the HTTP adapters and
//! the extension provider.
use crate::{StreamCallback, StreamEvent};
use std::time::Duration;
use tokio::time::Instant;

/// Pending text reaches the listener at least this often while it streams.
pub(super) const FLUSH_INTERVAL: Duration = Duration::from_millis(75);
/// Pending text reaches the listener once it holds this many characters.
pub(super) const FLUSH_CHARS: usize = 512;

/// Collects text and thinking fragments and hands them to the listener in
/// batches. A batch holds one kind, so the listener sees both in streamed order.
pub struct TextBatch {
    listener: StreamCallback,
    pending: String,
    thinking: bool,
    chars: usize,
    since: Option<Instant>,
}

impl TextBatch {
    pub fn new(listener: StreamCallback) -> Self {
        Self {
            listener,
            pending: String::new(),
            thinking: false,
            chars: 0,
            since: None,
        }
    }

    /// Queue a text fragment; delivers at once when the batch is full or old enough.
    pub fn push(&mut self, fragment: &str) {
        self.queue(false, fragment);
    }

    /// Queue a fragment of readable thinking, as `push` does for text.
    pub fn push_thinking(&mut self, fragment: &str) {
        self.queue(true, fragment);
    }

    fn queue(&mut self, thinking: bool, fragment: &str) {
        if fragment.is_empty() {
            return;
        }
        if self.thinking != thinking {
            self.flush();
            self.thinking = thinking;
        }
        let since = *self.since.get_or_insert_with(Instant::now);
        self.pending.push_str(fragment);
        self.chars += fragment.chars().count();
        if self.chars >= FLUSH_CHARS || since.elapsed() >= FLUSH_INTERVAL {
            self.flush();
        }
    }

    /// When pending text is due for delivery, if any is pending.
    pub fn due(&self) -> Option<Instant> {
        self.since.map(|since| since + FLUSH_INTERVAL)
    }

    /// Deliver pending text now: at block ends, at the terminal event, on errors.
    pub fn flush(&mut self) {
        self.since = None;
        self.chars = 0;
        if !self.pending.is_empty() {
            let text = std::mem::take(&mut self.pending);
            (self.listener)(if self.thinking {
                StreamEvent::ThinkingDelta(&text)
            } else {
                StreamEvent::TextDelta(&text)
            });
        }
    }
}
