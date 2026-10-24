//! Chat message builders shared by runner integration tests.
#![allow(dead_code)]
use bubblepaw_core::{Message, Role};

/// A message in the fixed `fixture-session` with the fixed id `fixture-message`.
pub fn fixture_message(role: Role, content: &str) -> Message {
    Message {
        id: "fixture-message".into(),
        session_id: "fixture-session".into(),
        role,
        origin: None,
        content: content.into(),
        tool_calls: Vec::new(),
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: String::new(),
    }
}

/// A user message with the given id in the `root` session.
pub fn root_message(id: &str, content: &str) -> Message {
    Message {
        id: id.into(),
        session_id: "root".into(),
        role: Role::User,
        origin: None,
        content: content.into(),
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: String::new(),
    }
}

/// A message with a fresh uuid id in `session`.
pub fn uuid_message(session: &str, role: Role, content: impl Into<String>) -> Message {
    Message {
        id: uuid::Uuid::new_v4().to_string(),
        session_id: session.into(),
        role,
        origin: None,
        content: content.into(),
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: String::new(),
    }
}
