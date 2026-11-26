//! What a provider adapter reports while a reply streams in.
use std::sync::Arc;

/// One live piece of a model reply. Adapters emit these in the order the
/// provider streams them; the runtime turns them into host events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StreamEvent<'a> {
    /// A batch of the visible reply text.
    TextDelta(&'a str),
    /// A batch of readable reasoning. Encrypted or redacted reasoning is never
    /// reported.
    ThinkingDelta(&'a str),
}

impl<'a> StreamEvent<'a> {
    /// The text of a `TextDelta`, nothing for any other event.
    pub fn text(self) -> Option<&'a str> {
        match self {
            Self::TextDelta(text) => Some(text),
            _ => None,
        }
    }
}

/// The listener every adapter emits its stream events through.
pub type StreamCallback = Arc<dyn for<'a> Fn(StreamEvent<'a>) + Send + Sync>;

/// A listener for the reply text only, the old text callback's behaviour.
pub fn text_callback(on_text: impl Fn(&str) + Send + Sync + 'static) -> StreamCallback {
    Arc::new(move |event| {
        if let StreamEvent::TextDelta(text) = event {
            on_text(text);
        }
    })
}
