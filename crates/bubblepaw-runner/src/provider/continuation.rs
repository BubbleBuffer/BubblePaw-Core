//! Joins a reply that a length stop split across several requests.
//!
//! A length stop continues by sending the cut text back as an assistant turn
//! followed by a user prompt. Claude 4.6 and later reject a prefilled final
//! assistant turn with thinking on or off, so the request always ends with the
//! user prompt. Partial tool calls are never continued: a reply that holds a
//! complete tool call is returned as it is, and the harness runs that call.
use bubblepaw_core::{Message, ModelRequest, ModelResponse, Role};

/// Continuation requests allowed after the first request of one reply.
pub(super) const MAX_CONTINUATIONS: usize = 4;

/// User turn that asks the model to pick up after a cut reply.
pub(super) const CONTINUE_PROMPT: &str = "Continue exactly where you stopped.";

/// Whether a cut reply should be continued. A reply with a complete tool call
/// is not continued, because continuing would need tool results first.
pub(super) fn should_continue(cut: bool, has_tool_calls: bool, continuations: usize) -> bool {
    cut && !has_tool_calls && continuations < MAX_CONTINUATIONS
}

/// The request for the next part of a cut reply: the partial text as an
/// assistant turn when there is any, then the continue prompt.
pub(super) fn continue_request(request: &ModelRequest, partial: &str) -> ModelRequest {
    let mut next = request.clone();
    let session_id = request
        .messages
        .last()
        .map(|message| message.session_id.clone())
        .unwrap_or_default();
    if !partial.is_empty() {
        next.messages
            .push(turn(Role::Assistant, partial, &session_id));
    }
    next.messages
        .push(turn(Role::User, CONTINUE_PROMPT, &session_id));
    next
}

/// Native continuation must return the partial assistant's complete opaque
/// state before the continue prompt, including a reasoning-only cut reply.
pub(super) fn continue_response(request: &ModelRequest, partial: &ModelResponse) -> ModelRequest {
    let mut next = request.clone();
    let session = request
        .messages
        .last()
        .map(|message| message.session_id.as_str())
        .unwrap_or(&request.session_id);
    if !partial.text.is_empty()
        || !partial.tool_calls.is_empty()
        || partial.provider_state.is_some()
    {
        let mut assistant = turn(Role::Assistant, &partial.text, session);
        assistant.tool_calls = partial.tool_calls.clone();
        assistant.provider_state = partial.provider_state.clone();
        next.messages.push(assistant);
    }
    next.messages
        .push(turn(Role::User, CONTINUE_PROMPT, session));
    next
}

/// Logs a reply that is still cut after the continuation limit. The reply is
/// returned with the text received so far; it is not an error.
pub(super) fn warn_incomplete(provider: &str, model: &str, continuations: usize) {
    tracing::warn!(
        target: "bubblepaw::provider",
        provider,
        model,
        continuations,
        "model reply still stopped on the output limit; returning the text received so far"
    );
}

fn turn(role: Role, content: &str, session_id: &str) -> Message {
    Message {
        id: String::new(),
        session_id: session_id.into(),
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
