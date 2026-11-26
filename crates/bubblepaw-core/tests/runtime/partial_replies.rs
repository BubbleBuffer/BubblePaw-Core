//! A request that fails or is cancelled after text streamed keeps that text as an incomplete reply.
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ContentBlock, Harness, HarnessConfig, IncompleteReason, Message,
    ModelRequest, ModelResponse, Provider, Role, RunOptions, Store, StreamCallback, StreamEvent,
    ToolRegistry,
};
use std::{collections::VecDeque, sync::Arc, sync::Mutex, time::Duration};
use tokio::sync::Notify;

#[derive(Clone, Copy)]
enum Ev {
    Text(&'static str),
    Think(&'static str),
    Discard,
}
#[derive(Clone, Copy)]
enum End {
    Fail,
    Hang,
    Reply(&'static str),
}
type Step = (Vec<Ev>, End);

/// Streams each request's events, then ends it as scripted.
struct Scripted {
    steps: Mutex<VecDeque<Step>>,
    requests: Mutex<Vec<ModelRequest>>,
    entered: Arc<Notify>,
}
impl Scripted {
    fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps.into()),
            requests: Mutex::default(),
            entered: Arc::new(Notify::new()),
        })
    }
}
#[async_trait]
impl Provider for Scripted {
    async fn generate(
        &self,
        request: ModelRequest,
        on_event: StreamCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        let (events, end) = self.steps.lock().unwrap().pop_front().expect("script");
        for event in events {
            on_event(match event {
                Ev::Text(text) => StreamEvent::TextDelta(text),
                Ev::Think(text) => StreamEvent::ThinkingDelta(text),
                Ev::Discard => StreamEvent::ThinkingDiscarded,
            });
        }
        match end {
            End::Fail => Err(anyhow!("boom")),
            End::Hang => {
                self.entered.notify_one();
                std::future::pending().await
            }
            End::Reply(text) => Ok(ModelResponse {
                text: text.into(),
                ..Default::default()
            }),
        }
    }
}

/// One run of `step` over a file store: its status and error, and the assistant
/// messages after a reload.
async fn run(step: Step) -> Result<(String, Option<String>, Vec<Message>)> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("chat.sqlite");
    let store = Store::open(&path)?;
    let session = store.create_session("partial")?;
    let hangs = matches!(step.1, End::Hang);
    let provider = Scripted::new(vec![step]);
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    let run = if hangs {
        harness.start(&session.id, "hello", RunOptions::default())?;
        provider.entered.notified().await;
        assert!(harness.cancel(&session.id));
        tokio::time::timeout(Duration::from_secs(5), harness.wait_for_idle(&session.id)).await??
    } else {
        harness
            .prompt(&session.id, "hello", RunOptions::default())
            .await?
    };
    drop(harness);
    drop(store);
    let replies = Store::open(&path)?
        .messages(&session.id)?
        .into_iter()
        .filter(|message| message.role == Role::Assistant)
        .collect();
    Ok((run.status, run.error, replies))
}

#[tokio::test]
async fn text_streamed_before_a_failure_or_cancel_is_kept_as_an_incomplete_reply() -> Result<()> {
    use Ev::{Discard, Text, Think};
    use IncompleteReason::{Cancelled, Error};
    // name, events, end, run status, kept reason, kept text, kept thinking
    #[rustfmt::skip]
    let cases = vec![
        ("fails after text", vec![Text("Hel"), Text("lo")], End::Fail, "failed", Some(Error), "Hello", ""),
        ("fails after thinking and text", vec![Think("why "), Think("so"), Text("Hi")], End::Fail, "failed", Some(Error), "Hi", "why so"),
        ("thinking of an abandoned attempt is dropped", vec![Think("old"), Discard, Text("Hi")], End::Fail, "failed", Some(Error), "Hi", ""),
        ("cancelled after text", vec![Text("part")], End::Hang, "cancelled", Some(Cancelled), "part", ""),
        ("fails after thinking only", vec![Think("hm")], End::Fail, "failed", None, "", ""),
        ("fails after blank text", vec![Text(" \n")], End::Fail, "failed", None, "", ""),
        ("retried before output, then answered", vec![Think("hm"), Discard, Text("Done")], End::Reply("Done"), "completed", None, "Done", ""),
    ];
    for (name, events, end, status, reason, text, thinking) in cases {
        let (run_status, error, replies) = run((events, end)).await?;
        assert_eq!(run_status, status, "{name}");
        // The outcome is the one the request ended with, whatever was kept.
        let expected_error = match status {
            "failed" => Some("boom"),
            "cancelled" => Some("Cancelled during provider request"),
            _ => None,
        };
        assert_eq!(error.as_deref(), expected_error, "{name}");
        match replies.as_slice() {
            [] => assert!(text.is_empty(), "{name}"),
            [reply] => {
                assert_eq!(reply.incomplete, reason, "{name}");
                assert_eq!(reply.text(), text, "{name}");
                assert_eq!(reply.thinking(), thinking, "{name}");
                assert!(reply.tool_calls.is_empty() && reply.provider_state.is_none());
                let first = reply.content.first();
                let thought = matches!(first, Some(ContentBlock::Thinking { .. }));
                assert_eq!(thought, !thinking.is_empty(), "{name}");
            }
            _ => panic!("{name}: {replies:?}"),
        }
    }
    Ok(())
}

#[tokio::test]
async fn the_next_request_carries_the_incomplete_reply_as_its_own_assistant_message() -> Result<()>
{
    let store = Store::in_memory()?;
    let session = store.create_session("next")?;
    let provider = Scripted::new(vec![
        (vec![Ev::Text("Hello")], End::Fail),
        (vec![], End::Reply("fine")),
    ]);
    let harness = Harness::new(
        store,
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    for prompt in ["one", "two"] {
        harness
            .prompt(&session.id, prompt, RunOptions::default())
            .await?;
    }
    let requests = provider.requests.lock().unwrap();
    let shape: Vec<_> = requests[1]
        .messages
        .iter()
        .map(|message| (message.role.clone(), message.text(), message.incomplete))
        .collect();
    assert_eq!(
        shape,
        vec![
            (Role::User, "one".into(), None),
            (
                Role::Assistant,
                "Hello".into(),
                Some(IncompleteReason::Error)
            ),
            (Role::User, "two".into(), None),
        ]
    );
    Ok(())
}

#[test]
fn a_message_without_the_field_is_complete_and_a_complete_one_writes_none() -> Result<()> {
    let mut json = serde_json::to_value(Message::default())?;
    assert!(json.get("incomplete").is_none(), "{json}");
    json["incomplete"] = "cancelled".into();
    let marked: Message = serde_json::from_value(json.clone())?;
    assert_eq!(marked.incomplete, Some(IncompleteReason::Cancelled));
    json.as_object_mut().unwrap().remove("incomplete");
    let old: Message = serde_json::from_value(json)?;
    assert_eq!(old.incomplete, None);
    Ok(())
}
