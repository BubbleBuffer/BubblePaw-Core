//! Prepared-payload accounting; canonical-only journals remain durable.
use crate::{Message, ModelRequest};
use anyhow::Result;
use serde_json::Value;

/// The prepared envelope budget includes native replay fields, but excludes
/// the Chat Completions opaque SSE journal which that adapter never replays.
/// This is not an exact count of an adapter's final HTTP representation.
pub fn prepared_request_bytes(request: &ModelRequest) -> Result<usize> {
    let mut bytes = serde_json::to_vec(request)?.len();
    for message in &request.messages {
        bytes -= canonical_only_bytes(message.provider_state.as_ref())?;
    }
    Ok(bytes)
}

pub(crate) fn prepared_message_bytes(message: &Message) -> Result<usize> {
    Ok(serde_json::to_vec(message)?.len() - canonical_only_bytes(message.provider_state.as_ref())?)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Role, Store};
    use serde_json::json;

    #[test]
    fn only_recognized_chat_journals_are_exempt_from_prepared_accounting() -> Result<()> {
        let mut message = Store::message("s", Role::Assistant, "visible".into());
        for state in [
            json!({"adapter":"chat-completions","message_fields":{"reasoning_content":"native"},"opaque_stream_fragments":[{"event":"archive"}]}),
            json!({"adapter":"chat-completions","rounds":[{"opaque_stream_fragments":[{"event":"a"}]},{"text_range":[0,7],"message_fields":{"reasoning_content":"native"},"opaque_stream_fragments":[]}]}),
        ] {
            message.provider_state = Some(state);
            let original = message.clone();
            let mut projected = message.clone();
            let state = projected.provider_state.as_mut().unwrap();
            state
                .as_object_mut()
                .unwrap()
                .remove("opaque_stream_fragments");
            if let Some(rounds) = state.get_mut("rounds").and_then(Value::as_array_mut) {
                for round in rounds {
                    round
                        .as_object_mut()
                        .unwrap()
                        .remove("opaque_stream_fragments");
                }
            }
            let request = ModelRequest {
                messages: vec![message.clone()],
                ..Default::default()
            };
            let projected_request = ModelRequest {
                messages: vec![projected.clone()],
                ..Default::default()
            };
            assert_eq!(
                prepared_message_bytes(&message)?,
                serde_json::to_vec(&projected)?.len()
            );
            assert_eq!(
                prepared_request_bytes(&request)?,
                serde_json::to_vec(&projected_request)?.len()
            );
            assert_eq!(message, original);
        }
        for state in [
            json!({"adapter":"other","opaque_stream_fragments":["must count"]}),
            json!({"opaque_stream_fragments":["must count"]}),
            json!({"adapter":"chat-completions","opaque_stream_fragments":"unknown encoding must count"}),
            json!({"adapter":"chat-completions","message_fields":{"opaque_stream_fragments":["actual native field must count"]}}),
        ] {
            message.provider_state = Some(state);
            assert_eq!(
                prepared_message_bytes(&message)?,
                serde_json::to_vec(&message)?.len()
            );
        }
        Ok(())
    }
}
