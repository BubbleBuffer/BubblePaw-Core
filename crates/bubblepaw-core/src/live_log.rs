//! The event log as the sink of live events: each live event is shown as the
//! event kind it has always been.
use crate::EventKind;
use crate::event_log::Log;
use crate::live_events::{LiveEvent, LiveEvents};

impl From<LiveEvent> for EventKind {
    fn from(event: LiveEvent) -> Self {
        match event {
            LiveEvent::Paragraph { text } => EventKind::Paragraph { text },
            LiveEvent::ModelRequestStarted => EventKind::ModelRequestStarted {},
            LiveEvent::TextDelta { text } => EventKind::TextDelta { text },
            LiveEvent::ThinkingDelta { text } => EventKind::ThinkingDelta { text },
            LiveEvent::ThinkingDiscarded => EventKind::ThinkingDiscarded {},
            LiveEvent::ToolStarted { call } => EventKind::ToolStarted { call },
            LiveEvent::ToolFinished {
                call_id,
                name,
                result,
            } => EventKind::ToolFinished {
                call_id,
                name,
                result,
            },
        }
    }
}

/// The event log broadcasts live events to every subscriber of the store.
impl LiveEvents for Log {
    fn transient(&self, session_id: &str, run_id: &str, event: LiveEvent) {
        Log::transient(self, session_id, run_id, event.into());
    }
    fn progress(&self, session_id: &str, run_id: &str, event: LiveEvent) {
        Log::progress(self, session_id, run_id, event.into());
    }
    fn paragraph(&self, session_id: &str, run_id: &str, text: String) {
        Log::paragraph(self, session_id, run_id, text);
    }
}
