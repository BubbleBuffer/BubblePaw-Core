use anyhow::{Result, anyhow};
use bubblepaw_core::{
    ContextClassification as Class, ContextState, MessageOrigin, ModelRequest, Reminder, Store,
};
use rusqlite::Connection;

fn request(store: &Store, session: &str) -> Result<ModelRequest> {
    Ok(ModelRequest {
        messages: store.messages(session)?,
        ..Default::default()
    })
}
fn build(
    state: &ContextState,
    store: &Store,
    session: &str,
) -> Result<bubblepaw_core::BuiltContext> {
    state.build_persisted(
        request(store, session)?,
        session,
        "root",
        &[],
        store,
        |_| Ok(()),
    )
}

#[test]
fn combines_latest_per_kind_in_one_generated_tail_and_suppresses_unchanged() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("reminders")?.id;
    let state = ContextState::default();
    let before = store.storage_stats()?.transaction_count;
    state.remind(&session, "root", "inventory", "old");
    state.remind(&session, "root", "budget", "two left");
    state.remind(&session, "root", "inventory", "latest\nraw text");
    assert_eq!(store.storage_stats()?.transaction_count, before);
    assert!(store.messages(&session)?.is_empty());
    let first = build(&state, &store, &session)?;
    assert_eq!(first.record.classification, Class::First);
    assert_eq!(first.request.messages.len(), 1);
    let tail = &first.request.messages[0];
    assert_eq!(tail.origin, Some(MessageOrigin::Generated));
    assert_eq!(
        tail.content,
        "<harness kind=\"inventory\">latest\nraw text</harness>\n<harness kind=\"budget\">two left</harness>"
    );
    assert_eq!(store.storage_stats()?.transaction_count, before + 1);
    let saved = serde_json::to_vec(tail)?;
    state.remind(&session, "root", "inventory", "latest\nraw text");
    state.remind(&session, "root", "budget", "two left");
    let repeated = build(&state, &store, &session)?;
    assert_eq!(repeated.record.classification, Class::Append);
    assert_eq!(serde_json::to_vec(&repeated.request.messages[0])?, saved);
    assert_eq!(store.messages(&session)?.len(), 1);
    assert_eq!(store.storage_stats()?.transaction_count, before + 1);
    let metadata = store.reminder_metadata(&session, &tail.id)?.unwrap();
    assert_eq!(metadata.reminders.len(), 2);
    assert_eq!(
        metadata.reminders[0].text_sha256,
        bubblepaw_core::fingerprint(b"latest\nraw text")
    );
    Ok(())
}

#[test]
fn preparation_and_storage_failures_preserve_pending_and_do_not_advance_hashes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open(directory.path().join("paw.db"))?;
    let session = store.create_session("failure")?.id;
    let state = ContextState::default();
    state.remind(&session, "root", "inventory", "retained");
    let before = store.storage_stats()?.chat_transaction_count;
    assert!(
        state
            .build_persisted(
                request(&store, &session)?,
                &session,
                "root",
                &[],
                &store,
                |_| Err(anyhow!("preparation/byte admission failure"))
            )
            .is_err()
    );
    assert!(store.messages(&session)?.is_empty());
    assert_eq!(store.storage_stats()?.chat_transaction_count, before);
    let conn = Connection::open(store.chat_path(&session)?.unwrap())?;
    conn.execute_batch("CREATE TRIGGER fail_reminder BEFORE INSERT ON reminder_metadata BEGIN SELECT RAISE(ABORT,'storage failure'); END;")?;
    assert!(build(&state, &store, &session).is_err());
    assert!(store.messages(&session)?.is_empty());
    assert_eq!(store.storage_stats()?.chat_transaction_count, before);
    conn.execute_batch("DROP TRIGGER fail_reminder;")?;
    let first = build(&state, &store, &session)?;
    assert_eq!(first.record.classification, Class::First);
    assert!(first.request.messages[0].content.contains("retained"));
    assert_eq!(store.messages(&session)?.len(), 1);
    Ok(())
}

#[test]
fn restart_dedup_survives_context_projection_dropping_old_reminder() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("paw.db");
    let store = Store::open(&path)?;
    let session = store.create_session("restart")?.id;
    let state = ContextState::default();
    state.remind(&session, "root", "inventory", "same");
    build(&state, &store, &session)?;
    let original = store.messages(&session)?;
    drop(state);
    drop(store);
    let store = Store::open(&path)?;
    let state = ContextState::default();
    state.remind(&session, "root", "inventory", "same");
    let compacted = state.build_persisted(
        ModelRequest::default(),
        &session,
        "root",
        &[],
        &store,
        |_| Ok(()),
    )?;
    assert!(compacted.request.messages.is_empty());
    assert_eq!(store.messages(&session)?, original);
    state.remind(&session, "root", "inventory", "changed");
    let changed = state.build_persisted(
        ModelRequest::default(),
        &session,
        "root",
        &[],
        &store,
        |_| Ok(()),
    )?;
    assert_eq!(changed.request.messages.len(), 1);
    assert_eq!(
        changed.request.messages[0].content,
        "<harness kind=\"inventory\">changed</harness>"
    );
    assert_eq!(store.messages(&session)?[0], original[0]);
    Ok(())
}

#[test]
fn lineages_and_discrete_delivery_identities_remain_independent() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("identity")?.id;
    let child = store.create_session("child")?.id;
    let state = ContextState::default();
    state.remind_with_metadata(
        &session,
        "root",
        Reminder::new("child_completion:a:run1", "done A")
            .with_delivery("child_completion", "[\"a\",\"run1\"]"),
    );
    state.remind_with_metadata(
        &session,
        "root",
        Reminder::new("child_completion:b:run2", "done B")
            .with_delivery("child_completion", "[\"b\",\"run2\"]"),
    );
    let built = build(&state, &store, &session)?;
    let metadata = store
        .reminder_metadata(&session, &built.request.messages[0].id)?
        .unwrap();
    assert_eq!(metadata.reminders.len(), 2);
    assert_eq!(metadata.reminders[1].deliveries[0].id, "[\"b\",\"run2\"]");
    state.remind(&child, "root", "inventory", "same");
    state.remind(&session, "fork", "inventory", "same");
    assert_eq!(
        state
            .build_persisted(
                ModelRequest::default(),
                &session,
                "fork",
                &[],
                &store,
                |_| Ok(())
            )?
            .request
            .messages
            .len(),
        1
    );
    assert_eq!(build(&state, &store, &child)?.request.messages.len(), 1);
    Ok(())
}

#[test]
fn compact_metadata_and_repeated_reminders_bound_writes_and_wal_growth() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open(directory.path().join("paw.db"))?;
    let session = store.create_session("size")?.id;
    let state = ContextState::default();
    let text = "long notice body\n".repeat(12_000);
    let before = store.storage_stats()?;
    state.remind(&session, "root", "inventory", &text);
    let built = build(&state, &store, &session)?;
    let after = store.storage_stats()?;
    assert_eq!(
        after.chat_transaction_count,
        before.chat_transaction_count + 1
    );
    let metadata = store
        .reminder_metadata(&session, &built.request.messages[0].id)?
        .unwrap();
    assert!(serde_json::to_vec(&metadata)?.len() < 512);
    for _ in 0..10 {
        state.remind(&session, "root", "inventory", &text);
        build(&state, &store, &session)?;
    }
    let repeated = store.storage_stats()?;
    assert_eq!(
        repeated.chat_transaction_count,
        after.chat_transaction_count
    );
    assert_eq!(repeated.wal_bytes, after.wal_bytes);
    assert_eq!(repeated.chat_bytes, after.chat_bytes);
    Ok(())
}

#[test]
fn fork_export_and_delete_preserve_then_clean_structured_metadata() -> Result<()> {
    let directory = tempfile::tempdir()?;
    for store in [
        Store::in_memory()?,
        Store::open(directory.path().join("paw.db"))?,
    ] {
        let session = store.create_session("source")?.id;
        let state = ContextState::default();
        state.remind_with_metadata(
            &session,
            "root",
            Reminder::new("question_answer:q", "answer").with_delivery("question_answer", "q"),
        );
        let built = build(&state, &store, &session)?;
        let metadata = store
            .reminder_metadata(&session, &built.request.messages[0].id)?
            .unwrap();
        let fork = store.fork(&session, "fork")?;
        let fork_message = &store.messages(&fork.id)?[0];
        assert_eq!(
            store
                .reminder_metadata(&fork.id, &fork_message.id)?
                .unwrap(),
            metadata
        );
        let mut exported = Vec::new();
        store.export_jsonl(&session, &mut exported)?;
        let record: serde_json::Value =
            serde_json::from_slice(exported.split(|byte| *byte == b'\n').next().unwrap())?;
        assert_eq!(
            record["reminder_metadata"]["reminders"][0]["deliveries"][0]["id"],
            "q"
        );
        store.delete_session(&session)?;
    }
    Ok(())
}
