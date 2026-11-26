//! Readable thinking: streamed as events, kept in the reply, never part of a request.
use super::reasoning_replay::{
    CHAT_MODEL, MESSAGES_MODEL, RESPONSES_MODEL, assistant, chat_reply, message, messages_reply,
    noop, reply, request, responses_reply, sse, start,
};
use bubblepaw_core::{
    CancellationToken, Provider, ReasoningReplay, Role, StreamCallback, StreamEvent,
};
use bubblepaw_runner::provider::{ChatCompletionsProvider, MessagesProvider, ResponsesProvider};
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

fn event_log() -> (StreamCallback, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    (
        Arc::new(move |event: StreamEvent| match event {
            StreamEvent::TextDelta(text) => sink.lock().unwrap().push(format!("text:{text}")),
            StreamEvent::ThinkingDelta(text) => {
                sink.lock().unwrap().push(format!("thinking:{text}"));
            }
            _ => {}
        }),
        seen,
    )
}

#[tokio::test]
async fn thinking_streams_in_order_and_the_next_request_is_what_main_sent() {
    let summary = |text: &str| json!({"type":"summary_text","text":text});
    let item = json!({"type":"reasoning","id":"rs-1","encrypted_content":"enc-1","summary":[summary("Plan."), summary("Read.")]});
    let responses = format!(
        "{}{}",
        sse(&[
            json!({"type":"response.reasoning_summary_part.added"}),
            json!({"type":"response.reasoning_summary_text.delta","delta":"Plan."}),
            json!({"type":"response.reasoning_summary_part.added"}),
            json!({"type":"response.reasoning_summary_text.delta","delta":"Read."}),
        ]),
        responses_reply(&item, "Answer"),
    );
    let signed = json!({"type":"thinking","thinking":"Plan.","signature":"sig-1"});
    let messages = messages_reply(&signed, "Answer");
    // (model, stream, marker of the replayed reasoning)
    let cases = [
        (CHAT_MODEL, chat_reply("Plan.\n\nRead.", "Answer"), "R"),
        (MESSAGES_MODEL, messages, "sig-1"),
        (RESPONSES_MODEL, responses, "enc-1"),
    ];
    for (model, stream, marker) in cases {
        let (base, mock) = start(vec![stream]).await;
        let provider: Arc<dyn Provider> = match model {
            CHAT_MODEL => Arc::new(
                ChatCompletionsProvider::api_key("synthetic-key", base)
                    .unwrap()
                    .with_reasoning_replay(BTreeMap::from([(model.into(), ReasoningReplay::All)]))
                    .unwrap(),
            ),
            MESSAGES_MODEL => Arc::new(MessagesProvider::api_key("synthetic-key", base).unwrap()),
            _ => Arc::new(ResponsesProvider::api_key("synthetic-key", base).unwrap()),
        };
        let (callback, seen) = event_log();
        let user = message(Role::User, "q");
        let response = provider
            .generate(
                request(model, vec![user.clone()]),
                callback,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let events = seen.lock().unwrap().clone();
        let first_text = events.iter().position(|e| e.starts_with("text:")).unwrap();
        assert!(
            first_text > 0,
            "{model}: thinking arrives before text: {events:?}"
        );
        assert!(
            events[..first_text]
                .iter()
                .all(|e| e.starts_with("thinking:"))
        );
        assert!(events[first_text..].iter().all(|e| e.starts_with("text:")));
        assert_eq!(response.text, "Answer", "{model}");
        assert!(response.thinking.starts_with("Plan."), "{model}");
        let joined: String = events[..first_text].iter().map(|e| &e[9..]).collect();
        assert_eq!(joined, response.thinking, "{model}");
        // What main stored: the same state, with Chat's readable reasoning in it.
        let mut main_state = response.provider_state.clone().unwrap();
        if model == CHAT_MODEL {
            main_state["message_fields"]["reasoning_content"] = json!(response.thinking);
        }
        let main = assistant(&response.text, Some(main_state));
        for assistant in [reply(response), main] {
            let history = vec![user.clone(), assistant, message(Role::User, "next")];
            let _ = provider
                .generate(request(model, history), noop(), CancellationToken::new())
                .await;
        }
        let requests = mock.requests.lock().unwrap();
        assert!(
            requests[1].to_string().contains(marker),
            "{model}: replayed"
        );
        assert_eq!(requests[1].to_string(), requests[2].to_string(), "{model}");
    }
}
