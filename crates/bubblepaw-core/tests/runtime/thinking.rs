//! A reply's readable thinking: a live event, then a stored block that model text ignores.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ContentBlock, EventKind, Harness, HarnessConfig, Message, ModelRequest,
    ModelResponse, Provider, Role, RunOptions, Store, StreamCallback, StreamEvent, ToolRegistry,
    prepared_request_bytes,
};
use std::sync::Arc;

struct Thinker;
#[async_trait]
impl Provider for Thinker {
    async fn generate(
        &self,
        _: ModelRequest,
        on_event: StreamCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        on_event(StreamEvent::ThinkingDelta("Plan the "));
        on_event(StreamEvent::ThinkingDelta("answer."));
        on_event(StreamEvent::TextDelta("Answer"));
        Ok(ModelResponse {
            text: "Answer".into(),
            thinking: "Plan the answer.".into(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn thinking_is_a_live_event_and_a_stored_block_model_text_ignores() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("chat.sqlite");
    let store = Store::open(&path)?;
    let session = store.create_session("thinking")?;
    let harness = Harness::new(
        store.clone(),
        Arc::new(Thinker),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    let mut events = harness.subscribe();
    harness
        .prompt(&session.id, "question", RunOptions::default())
        .await?;
    // The host event: live only, in order with the text deltas.
    let mut deltas = Vec::new();
    while let Ok(event) = events.try_recv() {
        match &event.kind {
            EventKind::ThinkingDelta { text } => {
                assert!(!event.durable);
                deltas.push(format!("thinking:{text}"));
            }
            EventKind::TextDelta { text, .. } => deltas.push(format!("text:{text}")),
            _ => {}
        }
    }
    assert_eq!(deltas.len(), 3, "{deltas:?}");
    assert_eq!(
        deltas[0].to_owned() + &deltas[1],
        "thinking:Plan the thinking:answer."
    );
    assert_eq!(deltas[2], "text:Answer");

    // The stored reply keeps its thinking block before the text, and a reload does too.
    drop(harness);
    drop(store);
    let reply = Store::open(&path)?
        .messages(&session.id)?
        .into_iter()
        .find(|message| message.role == Role::Assistant)
        .unwrap();
    assert!(
        matches!(&reply.content[0], ContentBlock::Thinking { text } if text == "Plan the answer.")
    );
    assert!(matches!(&reply.content[1], ContentBlock::Text { text } if text == "Answer"));
    assert_eq!(reply.thinking(), "Plan the answer.");

    // Model text is that of the reply without the block; the byte size adds the thinking.
    let mut plain = reply.clone();
    plain.content.remove(0);
    assert_eq!(reply.text(), plain.text());
    let size = |message: &Message| {
        prepared_request_bytes(&ModelRequest {
            messages: vec![message.clone()],
            ..Default::default()
        })
    };
    assert_eq!(size(&reply)?, size(&plain)? + "Plan the answer.".len());
    Ok(())
}
