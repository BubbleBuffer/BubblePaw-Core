//! The listener the router hands each attempt, and the thinking it must retract.
use crate::{StreamCallback, StreamEvent};
use anyhow::Result;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// `callback` that records whether text (`visible`) or thinking was streamed.
pub(crate) fn guard(
    callback: StreamCallback,
    visible: Arc<AtomicBool>,
) -> (StreamCallback, Arc<AtomicBool>) {
    let thinking = Arc::new(AtomicBool::new(false));
    let seen = thinking.clone();
    let guarded: StreamCallback = Arc::new(move |event| {
        match event {
            StreamEvent::TextDelta(text) if !text.is_empty() => {
                visible.store(true, Ordering::Release);
            }
            StreamEvent::ThinkingDelta(text) if !text.is_empty() => {
                seen.store(true, Ordering::Release);
            }
            _ => {}
        }
        callback(event);
    });
    (guarded, thinking)
}

/// A failed attempt that streamed thinking but no text is abandoned for a retry
/// or a fallback: tell the listener to drop that thinking.
pub(crate) fn discard_abandoned<T>(
    result: &Result<T>,
    published: &AtomicBool,
    thinking: &AtomicBool,
    listener: &StreamCallback,
) {
    if result.is_err()
        && !published.load(Ordering::Acquire)
        && thinking.swap(false, Ordering::AcqRel)
    {
        listener(StreamEvent::ThinkingDiscarded);
    }
}
