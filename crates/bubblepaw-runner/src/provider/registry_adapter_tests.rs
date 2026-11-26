//! The registry's reads of adapter output, which the adapters' own tests cannot make.
use super::*;
use crate::provider::MessagesProvider;
use serde_json::{Value, json};

fn reply(thinking: &str, state: Option<Value>) -> ModelResponse {
    ModelResponse {
        thinking: thinking.into(),
        provider_state: state,
        ..Default::default()
    }
}

#[test]
fn reply_reasoning_reads_state_first_then_thinking() {
    let state =
        json!({"adapter":"chat-completions","message_fields":{"reasoning_content":"thought"}});
    for response in [
        reply("thought", Some(state.clone())),
        reply("", Some(state.clone())),
        reply("thought", None),
        reply("other", Some(state)),
    ] {
        assert_eq!(
            ProviderRegistry::reply_reasoning(&response),
            Some("thought".into())
        );
    }
}

#[test]
fn messages_token_estimate_through_a_route_drops_foreign_signed_thinking() -> Result<()> {
    let provider = Arc::new(MessagesProvider::api_key(
        "synthetic-key",
        "http://127.0.0.1:1",
    )?);
    let mut assistant = bubblepaw_core::Message {
        id: "fixture-message".into(),
        session_id: "fixture-session".into(),
        role: bubblepaw_core::Role::Assistant,
        content: bubblepaw_core::text_blocks("done"),
        ..Default::default()
    };
    assistant.provider_state = Some(bubblepaw_core::with_provider_state_origin(
        json!({"adapter":"anthropic-messages","content":[
            {"type":"thinking","thinking":"readable thought","signature":"opaque signature"},
            {"type":"text","text":"done"}
        ]}),
        "anthropic-messages",
        Some("test"),
        "native",
    )?);
    assistant.provider_state.as_mut().unwrap()["_bubblepaw_route_origin"] =
        json!({"provider":"route", "endpoint":"http://127.0.0.1:2"});
    let request = ModelRequest {
        model: "route/test".into(),
        system_prompt: "stable instructions".into(),
        messages: vec![
            turn(bubblepaw_core::Role::User, "task"),
            assistant,
            turn(bubblepaw_core::Role::User, "next"),
        ],
        ..Default::default()
    };
    let mut registry = ProviderRegistry::new("route", provider);
    registry.configure_route("route", "http://127.0.0.1:1".into(), Default::default());
    assert_eq!(
        registry.estimate_input_tokens(&request, &|value| {
            assert!(!value.to_string().contains("readable thought"));
            456
        })?,
        Some(456)
    );
    Ok(())
}

#[test]
fn reported_cache_breakpoints_are_taken_off_the_reply_before_it_is_stored() {
    let marks = vec!["system.1".to_owned(), "messages.0.0".to_owned()];
    let mut response = reply(
        "",
        Some(json!({ bubblepaw_ai::BREAKPOINTS_KEY: marks.clone() })),
    );
    response.cache_breakpoints = Some(marks.clone());
    assert_eq!(
        bubblepaw_core::RequestLogRow::take_breakpoints(&mut response),
        Some(marks)
    );
    assert_eq!(response.provider_state, None, "the report is not stored");
}

fn turn(role: bubblepaw_core::Role, text: &str) -> bubblepaw_core::Message {
    bubblepaw_core::Message {
        id: format!("fixture-{text}"),
        session_id: "fixture-session".into(),
        role,
        content: bubblepaw_core::text_blocks(text),
        ..Default::default()
    }
}
