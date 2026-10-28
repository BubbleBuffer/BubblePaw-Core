use super::*;

#[test]
fn classical_captured_view_survives_checkpoint_advance_but_rejects_uncovered_or_partial_frontiers()
-> Result<()> {
    let store = Store::in_memory()?;
    let session =
        store.create_session_with_strategy("classical race", ContextStrategy::Classical)?;
    let first = Store::message(&session.id, Role::User, "first request".into());
    let mut assistant = Store::message(&session.id, Role::Assistant, String::new());
    assistant.tool_calls = vec![call("between-checkpoints")];
    let mut output = Store::message(&session.id, Role::Tool, "canonical full output".into());
    output.tool_call_id = Some("between-checkpoints".into());
    let future = Store::message(&session.id, Role::User, "not yet summarized".into());
    for message in [&first, &assistant, &output, &future] {
        store.append(message)?;
    }
    let original = serde_json::to_value(store.messages(&session.id)?)?;
    let mut checkpoint = ContextCheckpoint {
        id: id(),
        session_id: session.id.clone(),
        through_message_id: first.id.clone(),
        model: "summary".into(),
        summary: "captured first summary".into(),
        files_touched: Vec::new(),
        created_at: now(),
        usage: None,
    };
    let mut captured = ContextView {
        session_id: session.id.clone(),
        frontier: Some(store.message_seq(&session.id, &first.id)?),
        system_prompt: "frozen instructions".into(),
        prefix: vec![Store::message(
            &session.id,
            Role::System,
            checkpoint.summary.clone(),
        )],
        masked_tool_output_ids: Vec::new(),
        clipped_tool_call_ids: Vec::new(),
        created_at: now(),
        threshold: 200,
        budget: 100,
        tokens: 20,
        blocks: Vec::new(),
        blocks_estimated: true,
    };
    assert!(store.save_context_view(&captured).is_err());
    store.save_context_checkpoint(&checkpoint)?;
    // The worker replaces C1 after preparation captured its summary and frontier.
    checkpoint.id = id();
    checkpoint.through_message_id = output.id.clone();
    checkpoint.summary = "newer second summary".into();
    store.save_context_checkpoint(&checkpoint)?;
    store.save_context_view(&captured)?;
    let saved = serde_json::to_value(&captured)?;
    assert_eq!(
        store.context_checkpoint(&session.id)?.unwrap().summary,
        "newer second summary"
    );
    for invalid in [&assistant, &future] {
        captured.frontier = Some(store.message_seq(&session.id, &invalid.id)?);
        assert!(store.save_context_view(&captured).is_err());
        assert_eq!(
            serde_json::to_value(store.context_view(&session.id)?.unwrap())?,
            saved
        );
    }
    let covered_frontier = Some(store.message_seq(&session.id, &output.id)?);
    let before = store.storage_stats()?.transaction_count;
    for invalid in [
        ContextView {
            frontier: covered_frontier,
            masked_tool_output_ids: vec![output.id.clone()],
            ..captured.clone()
        },
        ContextView {
            frontier: covered_frontier,
            clipped_tool_call_ids: vec!["between-checkpoints".into()],
            ..captured.clone()
        },
    ] {
        assert!(store.save_context_view(&invalid).is_err());
        assert_eq!(store.storage_stats()?.transaction_count, before);
        assert_eq!(
            serde_json::to_value(store.context_view(&session.id)?.unwrap())?,
            saved
        );
    }
    assert_eq!(
        serde_json::to_value(store.messages(&session.id)?)?,
        original
    );
    Ok(())
}
