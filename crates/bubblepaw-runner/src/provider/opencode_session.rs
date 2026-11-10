//! The `x-opencode-session` header value and the Codex prompt-cache key.
use bubblepaw_core::ModelRequest;

/// Session id for the `x-opencode-session` header, chosen the same way for
/// every wire that sends it: the request's session, else its first message's
/// session, else its first message id, else a fresh UUID.
pub(crate) fn opencode_session(request: &ModelRequest) -> String {
    session_key(request).unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

/// The stable session identity of a request, if it has one. A fork that
/// cleared `session_id` still recovers it from its messages.
pub(crate) fn session_key(request: &ModelRequest) -> Option<String> {
    (!request.session_id.is_empty())
        .then_some(request.session_id.as_str())
        .or_else(|| {
            request
                .messages
                .iter()
                .find(|message| !message.session_id.is_empty())
                .map(|message| message.session_id.as_str())
        })
        .or_else(|| {
            request
                .messages
                .iter()
                .find(|message| !message.id.is_empty())
                .map(|message| message.id.as_str())
        })
        .map(str::to_owned)
}
