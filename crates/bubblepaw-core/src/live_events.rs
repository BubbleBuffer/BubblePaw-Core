//! How the run loop and the dispatcher show what is happening right now. Live
//! events reach subscribers only: they are never stored, and the durable events
//! stay with the journal.
use crate::{EventKind, event_log::Log};

pub(crate) trait LiveEvents: Send + Sync {
    /// A live event outside the streamed text, such as a tool starting or finishing.
    fn transient(&self, session_id: &str, run_id: &str, kind: EventKind);
    /// One streamed text or thinking batch.
    fn progress(&self, session_id: &str, run_id: &str, kind: EventKind);
    /// One finished paragraph of streamed text.
    fn paragraph(&self, session_id: &str, run_id: &str, text: String);
}

/// The event log broadcasts live events to every subscriber of the store.
impl LiveEvents for Log {
    fn transient(&self, session_id: &str, run_id: &str, kind: EventKind) {
        Log::transient(self, session_id, run_id, kind);
    }
    fn progress(&self, session_id: &str, run_id: &str, kind: EventKind) {
        Log::progress(self, session_id, run_id, kind);
    }
    fn paragraph(&self, session_id: &str, run_id: &str, text: String) {
        Log::paragraph(self, session_id, run_id, text);
    }
}
