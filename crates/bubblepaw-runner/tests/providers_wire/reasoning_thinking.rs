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
        // What main stored: the same state and text, without the thinking block.
        let main = assistant(&response.text, response.provider_state.clone());
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

/// Streams thinking and fails transiently on the first attempt, then succeeds with other thinking.
struct FlakyThinker(std::sync::atomic::AtomicUsize);
#[async_trait::async_trait]
impl Provider for FlakyThinker {
    async fn generate(
        &self,
        _: bubblepaw_core::ModelRequest,
        on_event: StreamCallback,
        _: CancellationToken,
    ) -> anyhow::Result<bubblepaw_core::ModelResponse> {
        if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            on_event(StreamEvent::ThinkingDelta("failed plan"));
            return Err(bubblepaw_core::ProviderError::new(
                bubblepaw_core::ProviderErrorKind::Transient,
                "dropped",
            )
            .into());
        }
        on_event(StreamEvent::ThinkingDelta("good plan"));
        on_event(StreamEvent::TextDelta("Answer"));
        Ok(bubblepaw_core::ModelResponse {
            text: "Answer".into(),
            thinking: "good plan".into(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn an_abandoned_attempt_discards_its_thinking_and_only_the_retry_is_stored() {
    use bubblepaw_core::{EventKind, Harness, HarnessConfig, RunOptions, Store, ToolRegistry};
    let policy = bubblepaw_runner::provider::RetryPolicy {
        max_attempts: 2,
        initial_delay: std::time::Duration::ZERO,
        max_delay: std::time::Duration::ZERO,
    };
    let registry = bubblepaw_runner::provider::ProviderRegistry::new(
        "p",
        Arc::new(FlakyThinker(Default::default())),
    )
    .with_retry_policy(policy);
    let store = Store::in_memory().unwrap();
    let session = store.create_session("retry").unwrap();
    let harness = Harness::new(
        store.clone(),
        Arc::new(registry),
        ToolRegistry::new(),
        HarnessConfig {
            model: "p/m".into(),
            ..Default::default()
        },
    );
    let mut events = harness.subscribe();
    harness
        .prompt(&session.id, "q", RunOptions::default())
        .await
        .unwrap();
    let mut seen = Vec::new();
    while let Ok(event) = events.try_recv() {
        match event.kind {
            EventKind::ThinkingDelta { text } => seen.push(format!("thinking:{text}")),
            EventKind::ThinkingDiscarded {} => seen.push("discarded".into()),
            EventKind::TextDelta { text } => seen.push(format!("text:{text}")),
            _ => {}
        }
    }
    assert_eq!(
        seen,
        [
            "thinking:failed plan",
            "discarded",
            "thinking:good plan",
            "text:Answer"
        ]
    );
    let stored = store.messages(&session.id).unwrap();
    assert_eq!(stored.last().unwrap().thinking(), "good plan");
}
