//! Tracking whether a reply's text reached the listener, and marking an error
//! with it. Shared by the HTTP adapters and the extension provider.
use crate::{ProviderError, ProviderErrorKind, StreamCallback, StreamEvent};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Publication follows listener delivery, including prior continuation rounds.
pub fn track_text_publication(on_text: StreamCallback) -> (StreamCallback, Arc<AtomicBool>) {
    let published = Arc::new(AtomicBool::new(false));
    let signal = published.clone();
    let listener: StreamCallback = Arc::new(move |event| {
        if matches!(event, StreamEvent::TextDelta(text) if !text.is_empty()) {
            signal.store(true, Ordering::Release);
        }
        on_text(event);
    });
    (listener, published)
}

/// Preserve the error chain so usage and provider Retry-After remain available.
pub fn mark_published(error: anyhow::Error, published: bool) -> anyhow::Error {
    if !published {
        return error;
    }
    let typed = ProviderError::of(&error)
        .cloned()
        .unwrap_or_else(|| ProviderError::new(ProviderErrorKind::Fatal, error.to_string()));
    if typed.published {
        error
    } else {
        error.context(typed.with_published(true))
    }
}
