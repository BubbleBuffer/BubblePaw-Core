//! Imported coverage keeps unknown source sizes explicit.
use super::*;

fn seed_with_inaccessible_original(f: &Fixture) -> Result<Vec<Message>> {
    let original = vec![
        f.message(Role::User, "Original imported task"),
        f.message(Role::Assistant, "Inherited original body"),
    ];
    let seed = bubblepaw_core::transcript::ImportDerivedRecord {
        id: "source-compaction".into(),
        session_id: f.session.clone(),
        source_line: 3,
        source_kind: "codex_compaction".into(),
        through_message_id: Some(original[1].id.clone()),
        content: json!({"message":"Unchanged imported memory."}),
        created_at: "2026-10-12T00:00:00Z".into(),
        origin: "import".into(),
    };
    f.store.append_import_batch(&original, &[seed])?;
    // A source count must not depend on decoding an already summarized body.
    // Keep the opening anchor readable; deliberately make the other old row
    // unreadable, as the existing cold-import continuation regression does.
    let connection = rusqlite::Connection::open(f.store.chat_path(&f.session)?.unwrap())?;
    connection.execute(
        "UPDATE messages SET data=?1 WHERE message_id=?2",
        rusqlite::params!["inaccessible inherited body", original[1].id],
    )?;
    assert!(f.store.messages(&f.session).is_err());
    Ok(original)
}

fn imported_policy(f: &Fixture) -> Result<Arc<SummarizingContext>> {
    Ok(Arc::new(SummarizingContext::new(
        f.store.clone(),
        Arc::new(NoMain),
        SummaryConfig {
            threshold_cap: 5_000,
            lcm: LcmConfig {
                output_reserve_tokens: 0,
                ..Default::default()
            },
            ..Default::default()
        },
        None,
    )?))
}

#[tokio::test]
async fn imported_and_native_apply_keeps_full_range_and_unknown_source_count() -> Result<()> {
    let f = Fixture::new(ContextStrategy::Lcm)?;
    let imported = seed_with_inaccessible_original(&f)?;
    let fresh = vec![
        f.message(Role::User, "New native request"),
        f.message(Role::Assistant, " datum".repeat(34_000)),
        f.message(Role::User, "Latest request"),
    ];
    f.store.append_batch(&fresh)?;
    let context = imported_policy(&f)?;
    let worker = new_episode_worker(f.store.clone(), f.models.clone(), context.clone());
    context.set_episode_worker(&worker);
    worker.precompute(&f.session).await?;
    let prepared = prepare_fresh(
        &*context,
        Arc::new(NoMain),
        &f.session,
        ModelRequest {
            messages: f.store.serving_messages(&f.session)?,
            ..Default::default()
        },
    )
    .await?;
    let events = f.store.context_events(&f.session)?;
    let event = events
        .iter()
        .find(|event| event.kind == ContextEventKind::Applied)
        .unwrap();
    let episodes = f.store.context_episodes(&f.session)?;
    assert_eq!(event.first_message_id, imported[0].id);
    assert_eq!(event.last_message_id, fresh[1].id);
    assert_eq!(event.tokens_before, None);
    assert_eq!(event.episode_ids, vec![episodes[0].id.clone()]);
    let tokenizer = tiktoken_rs::bpe_for_tokenizer(tiktoken_rs::tokenizer::Tokenizer::O200kBase)?;
    assert_eq!(
        event.tokens_after,
        (tokenizer
            .encode_ordinary("Unchanged imported memory.")
            .len()
            + tokenizer.encode_ordinary(&episodes[0].summary).len()) as u64
    );
    assert!(
        prepared
            .request
            .messages
            .iter()
            .any(|message| message.content == "Unchanged imported memory.")
    );
    assert!(f.store.messages(&f.session).is_err());
    let view = f.store.context_view(&f.session)?.unwrap();
    assert_blocks(&view, &prepared.request);
    assert_eq!(view.blocks[1].kind, ContextBlockKind::ChapterSummary);
    assert!(view.blocks[1].tokens > 0);
    assert!(view.blocks[1].id.is_none());
    assert!(view.blocks[1].first_message_id.is_none());
    assert!(view.blocks[1].last_message_id.is_none());
    Ok(())
}

#[tokio::test]
async fn imported_seed_only_publication_still_records_full_applied_event() -> Result<()> {
    let f = Fixture::new(ContextStrategy::Lcm)?;
    let imported = seed_with_inaccessible_original(&f)?;
    f.store.append_batch(&[
        f.message(Role::User, "Current task"),
        f.message(Role::Assistant, "Current response"),
    ])?;
    let context = imported_policy(&f)?;
    let request = ModelRequest {
        messages: f.store.serving_messages(&f.session)?,
        ..Default::default()
    };
    let raw = context.estimated_tokens(&request);
    prepare_fresh(&*context, Arc::new(NoMain), &f.session, request.clone()).await?;
    assert!(f.store.context_view(&f.session)?.is_none());
    bubblepaw_core::ContextPreparation::observe_response(
        &*context,
        &f.session,
        &ModelResponse {
            usage: Some(Usage {
                input_tokens: 7_000,
                output_tokens: 0,
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    let prepared = prepare_fresh(&*context, Arc::new(NoMain), &f.session, request).await?;
    let events = f.store.context_events(&f.session)?;
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.kind, ContextEventKind::Applied);
    assert_eq!(event.trigger, ContextTrigger::Pressure);
    assert_eq!(event.first_message_id, imported[0].id);
    assert_eq!(event.last_message_id, imported[1].id);
    assert_eq!(event.tokens_before, None);
    assert_eq!(event.input_before, Some(raw as u64));
    assert_eq!(
        event.input_after,
        Some(context.estimated_tokens(&prepared.request) as u64)
    );
    assert!(event.episode_ids.is_empty());
    assert!(event.summary_id.is_none());
    assert!(f.models.inputs.lock().unwrap().is_empty());
    assert!(f.store.messages(&f.session).is_err());
    assert_blocks(
        &f.store.context_view(&f.session)?.unwrap(),
        &prepared.request,
    );
    Ok(())
}
