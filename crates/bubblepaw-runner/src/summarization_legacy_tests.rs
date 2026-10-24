use super::*;
use bubblepaw_core::ToolCall;

#[tokio::test]
async fn legacy_view_serves_its_frozen_system_prompt_and_prefix_unchanged() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let session = store.create_session("frozen inventory")?.id;
    let repl = Arc::new(crate::codemode::CodeMode::new(
        directory.path().to_owned(),
        Arc::new(crate::artifacts::ArtifactStore::new(
            directory.path(),
            directory.path().join("artifacts"),
        )?),
    )?);
    let notes = Arc::new(RuntimeNotes::without_processes(repl));
    let context = SummarizingContext::new(
        store.clone(),
        Arc::new(crate::provider::ScriptedProvider::new("unused")),
        SummaryConfig::default(),
        Some(notes),
    )?
    .with_auto_summarize(false);
    assert!(!has_inventory(
        &built_after_prepare(
            &context,
            Arc::new(crate::provider::ScriptedProvider::new("unused")),
            &store,
            &session,
        )
        .await?,
    ));
    let mut old = SummarizingContext::synthetic(
        &session,
        Role::User,
        "[host notice]\nCurrent volatile runtime facts: stale heap".into(),
    );
    old.origin = Some(bubblepaw_core::MessageOrigin::Generated);
    old.created_at = "2099-01-01T00:00:00Z".into();
    let mut assistant = SummarizingContext::synthetic(&session, Role::Assistant, String::new());
    assistant.tool_calls.push(ToolCall {
        id: "call".into(),
        name: "read".into(),
        arguments: json!({"path":"a"}),
    });
    let mut result = SummarizingContext::synthetic(&session, Role::Tool, "complete result".into());
    result.tool_call_id = Some("call".into());
    let history = vec![old.clone(), assistant.clone(), result.clone()];
    store.append_batch(&history)?;
    // A view saved before P4: frozen system prompt, inventory and lookup in its prefix.
    let mut inventory = SummarizingContext::synthetic(
        &session,
        Role::User,
        "[host notice]\nCurrent volatile runtime facts: frozen".into(),
    );
    inventory.origin = Some(bubblepaw_core::MessageOrigin::Generated);
    let lookup =
        SummarizingContext::synthetic(&session, Role::System, HISTORY_RETRIEVAL_INSTRUCTION.into());
    let view = ContextView {
        session_id: session.clone(),
        system_prompt: "stable system".into(),
        frontier: None,
        prefix: vec![lookup, inventory],
        masked_tool_output_ids: vec![],
        clipped_tool_call_ids: vec![],
        created_at: now(),
        threshold: 100000,
        budget: 50000,
        tokens: 0,
    };
    store.save_context_view(&view)?;
    let mut later = old.clone();
    later.id = id();
    later.created_at = "1900-01-01T00:00:00Z".into();
    later.content = "[host notice]\nCurrent volatile runtime facts: newer inventory".into();
    let mut extended = history.clone();
    extended.push(later.clone());
    let prepared = context.render_saved(&session, &extended, &view)?;
    let mut expected = view.prefix.clone();
    expected.extend([old, assistant, result, later]);
    assert_eq!(prepared, expected);
    let request = ModelRequest {
        system_prompt: "current system".into(),
        messages: history.clone(),
        ..Default::default()
    };
    let frozen = context.frozen_request(&session, request)?;
    assert_eq!(frozen.system_prompt, "stable system");
    assert_eq!(frozen.messages, prepared[..5]);
    assert_eq!(context.render_saved(&session, &extended, &view)?, prepared);
    let mut snapshot = HistorySnapshot::index(&store, &session)?;
    snapshot.scan(0, &context)?;
    assert_eq!(
        context.render_cold_saved(&session, &snapshot, &view)?,
        prepared[..5]
    );
    assert_eq!(
        store.messages(&session)?,
        history,
        "canonical notices are retained intact"
    );
    let (state, hooked) = prepare_main(
        &context,
        Arc::new(crate::provider::ScriptedProvider::new("unused")),
        &session,
        ModelRequest {
            messages: store.messages(&session)?,
            ..Default::default()
        },
    )
    .await?;
    let cold = context.cold_message_indexes(&session, &hooked.request)?;
    let mut placed = hooked.request.clone();
    state.place_cold_notes(&mut placed, &session, "root", &cold);
    assert_eq!(
        placed.messages.len(),
        hooked.request.messages.len(),
        "legacy prefix keeps its lookup"
    );
    assert!(has_inventory(
        &built_after_prepare(
            &context,
            Arc::new(crate::provider::ScriptedProvider::new("unused")),
            &store,
            &session,
        )
        .await?,
    ));
    Ok(())
}
