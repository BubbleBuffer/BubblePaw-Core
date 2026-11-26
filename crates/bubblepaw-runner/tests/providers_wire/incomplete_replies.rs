//! An incomplete assistant reply goes to every adapter as an ordinary assistant text message, between the user turns around it.
use super::reasoning_replay::{
    CHAT_MODEL, MESSAGES_MODEL, RESPONSES_MODEL, assistant, chat_reply, message, messages_reply,
    noop, request, responses_reply, start,
};
use bubblepaw_core::{CancellationToken, IncompleteReason, Provider, Role};
use bubblepaw_runner::provider::{ChatCompletionsProvider, MessagesProvider, ResponsesProvider};
use serde_json::{Value, json};
use std::sync::Arc;

/// Every string a wire item carries under a `text` key or as its `content`.
fn texts(item: &Value, out: &mut Vec<String>) {
    match item {
        Value::String(_) => {}
        Value::Array(items) => items.iter().for_each(|item| texts(item, out)),
        Value::Object(fields) => {
            for (key, value) in fields {
                match (key.as_str(), value) {
                    ("text" | "content", Value::String(text)) => out.push(text.clone()),
                    _ => texts(value, out),
                }
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn the_next_request_sends_an_incomplete_reply_as_a_plain_assistant_message() {
    let thinking = json!({"type":"thinking","thinking":"","signature":"s"});
    let item = json!({"type":"reasoning","id":"rs","encrypted_content":"e","summary":[]});
    // (model, reply to the probe request, where the wire keeps its turns)
    let cases = [
        (CHAT_MODEL, chat_reply("", "ok"), "messages"),
        (MESSAGES_MODEL, messages_reply(&thinking, "ok"), "messages"),
        (RESPONSES_MODEL, responses_reply(&item, "ok"), "input"),
    ];
    for (model, stream, turns) in cases {
        let (base, mock) = start(vec![stream]).await;
        let provider: Arc<dyn Provider> = match model {
            CHAT_MODEL => {
                Arc::new(ChatCompletionsProvider::api_key("synthetic-key", base).unwrap())
            }
            MESSAGES_MODEL => Arc::new(MessagesProvider::api_key("synthetic-key", base).unwrap()),
            _ => Arc::new(ResponsesProvider::api_key("synthetic-key", base).unwrap()),
        };
        // What the loop stores for a request cut off after thinking and "Hel".
        let mut cut = assistant("", None);
        cut.content = bubblepaw_core::reply_blocks("why", "Hel");
        cut.incomplete = Some(IncompleteReason::Error);
        let history = vec![message(Role::User, "q"), cut, message(Role::User, "next")];
        provider
            .generate(request(model, history), noop(), CancellationToken::new())
            .await
            .unwrap_or_else(|error| panic!("{model}: {error:#}"));
        let body = mock.requests.lock().unwrap()[0].clone();
        let mut shape = Vec::new();
        for item in body[turns].as_array().unwrap() {
            let role = item["role"].as_str().unwrap_or_default();
            if role == "user" || role == "assistant" {
                let mut found = Vec::new();
                texts(item, &mut found);
                shape.push((role.to_owned(), found.concat()));
            }
        }
        let expected = [("user", "q"), ("assistant", "Hel"), ("user", "next")]
            .map(|(role, text)| (role.to_owned(), text.to_owned()));
        assert_eq!(shape, expected, "{model}: {body}");
        assert!(!body.to_string().contains("incomplete"), "{model}");
    }
}
