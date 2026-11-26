//! Prepared-payload accounting tests; they build messages through the store.
use crate::{ModelRequest, Role, Store, prepared_message_bytes, prepared_request_bytes};
use anyhow::Result;
use serde_json::{Value, json};

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
            crate::plain_content(|| serde_json::to_vec(&projected))?.len()
        );
        assert_eq!(
            prepared_request_bytes(&request)?,
            crate::plain_content(|| serde_json::to_vec(&projected_request))?.len()
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
            crate::plain_content(|| serde_json::to_vec(&message))?.len()
        );
    }
    Ok(())
}
