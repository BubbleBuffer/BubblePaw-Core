//! Stored chat message fixture for context-strategy tests.
use bubblepaw_core::{Message, Role};

pub fn message(id: &str, role: Role, content: String) -> Message {
    Message {
        id: id.into(),
        session_id: "fixture".into(),
        role,
        content,
        tool_calls: vec![],
        tool_call_id: None,
        origin: None,
        provider_state: None,
        is_error: false,
        created_at: "2026-10-09T00:00:00Z".into(),
    }
}
