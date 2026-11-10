//! Summary forks over the extension protocol (docs/providers.md): the
//! `summary_fork` request an extension answers, the source mapping and the size
//! projection the host's fork admission uses.
use anyhow::{Result, ensure};
use bubblepaw_core::{Message, MessageOrigin, ModelRequest, Role, is_ephemeral};
use serde_json::{Value, json};

/// The fork request as the extension gets it. The host's controls (the branch
/// instruction, then the return control) follow the last message of the captured
/// conversation, the last one with an id. Claude Code has no system-role tail,
/// so they travel as one user message, in that order, and the request names the
/// session the fork branches (the host clears it on every fork request).
pub(super) fn request(mut request: ModelRequest) -> Result<ModelRequest> {
    let cut = request
        .messages
        .iter()
        .rposition(|message| !message.id.is_empty());
    let Some(cut) = cut else {
        anyhow::bail!("a summary fork needs a conversation to branch");
    };
    let controls = request.messages.split_off(cut + 1);
    ensure!(
        !controls.is_empty()
            && controls
                .iter()
                .all(|message| matches!(message.role, Role::System | Role::User)),
        "a summary fork request ends in the host's branch controls"
    );
    let session = controls[controls.len() - 1].session_id.clone();
    let content = controls
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    request.messages.push(Message {
        id: String::new(),
        session_id: session.clone(),
        role: Role::User,
        origin: Some(MessageOrigin::Generated),
        content,
        tool_calls: Vec::new(),
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: String::new(),
    });
    request.session_id = session;
    Ok(request)
}

/// Source message ids mapped to the message's ordinal in the conversation.
/// Claude Code has no wire message table of its own: the extension rebuilds the
/// conversation from these messages, one for one, and runtime notes (ephemeral
/// messages) ride along with a turn and have no position.
pub(super) fn mapping(request: &ModelRequest) -> Value {
    let mut ordinal = 0_usize;
    let sources: Vec<Value> = request
        .messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let positions: Vec<usize> = if is_ephemeral(message) {
                Vec::new()
            } else {
                ordinal += 1;
                vec![ordinal - 1]
            };
            json!({
                "source_id":message.id,"prepared_position":index,"role":message.role,
                "origin":message.origin,"is_typed_user":bubblepaw_core::is_typed_user(message),
                "wire_positions":positions,
                "tool_call_ids":message.tool_calls.iter().map(|call| &call.id).collect::<Vec<_>>(),
                "tool_call_id":message.tool_call_id,
            })
        })
        .collect();
    json!({
        "protocol":"claude-code",
        "position_unit":"zero-based ordinal of the message in the conversation, counting every message except runtime notes; Claude Code has no wire message table",
        "sources":sources,
    })
}

/// What the extension is sent for the host's local size estimate: the system
/// prompt, tools and messages, without the native provider state (opaque blocks
/// that are never tokenized).
pub(super) fn projection(request: &ModelRequest) -> Value {
    let messages: Vec<Message> = request
        .messages
        .iter()
        .map(|message| Message {
            provider_state: None,
            ..message.clone()
        })
        .collect();
    json!({"system":request.system_prompt,"tools":request.tools,"messages":messages})
}
