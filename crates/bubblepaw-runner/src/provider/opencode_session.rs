//! The `x-opencode-session` header value.
use bubblepaw_core::ModelRequest;

/// Session id for the `x-opencode-session` header, chosen the same way for
/// every wire that sends it: the request's session, else its first message's
/// session, else its first message id, else a fresh UUID.
pub(crate) fn opencode_session(request: &ModelRequest) -> String {
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
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}
