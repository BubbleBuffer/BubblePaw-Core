//! Prepared-payload accounting; canonical-only journals remain durable.
use crate::{Message, ModelRequest};
use anyhow::Result;
use serde_json::Value;

/// The prepared envelope budget includes native replay fields, but excludes
/// the Chat Completions opaque SSE journal which that adapter never replays.
/// This is not an exact count of an adapter's final HTTP representation.
pub fn prepared_request_bytes(request: &ModelRequest) -> Result<usize> {
    let mut bytes = crate::plain_content(|| serde_json::to_vec(request))?.len();
    for message in &request.messages {
        bytes -= canonical_only_bytes(message.provider_state.as_ref())?;
    }
    Ok(bytes)
}

#[doc(hidden)]
pub fn prepared_message_bytes(message: &Message) -> Result<usize> {
    Ok(crate::plain_content(|| serde_json::to_vec(message))?.len()
        - canonical_only_bytes(message.provider_state.as_ref())?)
}

fn journal_field_bytes(state: &Value) -> Result<usize> {
    let Some(object) = state.as_object() else {
        return Ok(0);
    };
    let Some(journal) = object
        .get("opaque_stream_fragments")
        .filter(|value| value.is_array())
    else {
        return Ok(0);
    };
    // The field's quoted key, colon, value and one separating comma. A round
    // containing only the journal has no comma; retain its empty braces.
    Ok(serde_json::to_vec("opaque_stream_fragments")?.len()
        + 1
        + serde_json::to_vec(journal)?.len()
        + usize::from(object.len() > 1))
}

fn canonical_only_bytes(state: Option<&Value>) -> Result<usize> {
    let Some(state) = state.filter(|state| state["adapter"] == "chat-completions") else {
        return Ok(0);
    };
    let mut bytes = journal_field_bytes(state)?;
    // Continuation rounds inherit the enclosing adapter, and intentionally
    // omit their own adapter/origin tags. No arbitrary nested fields are exempt.
    if let Some(rounds) = state["rounds"].as_array() {
        for round in rounds {
            bytes += journal_field_bytes(round)?;
        }
    }
    Ok(bytes)
}
