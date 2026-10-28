use super::*;
use bubblepaw_core::{
    ContextBlock, ContextBlockKind, ContextEvent, ContextEventKind, ContextTrigger,
};
use rusqlite::Connection;

#[path = "notifications.rs"]
mod notifications;

fn event(batch: &EpisodeBatch, id: &str, kind: ContextEventKind) -> ContextEvent {
    ContextEvent {
        id: id.into(),
        session_id: batch.episode.session_id.clone(),
        kind,
        created_at: "2026-10-27T00:00:00Z".into(),
        trigger: ContextTrigger::SourceSize,
        trigger_text: "Source target reached".into(),
        first_message_id: batch.episode.first_message_id.clone(),
        last_message_id: batch.episode.last_message_id.clone(),
        episode_ids: vec![batch.episode.id.clone()],
        summary_id: Some(batch.episode.id.clone()),
        tokens_before: Some(32_000),
        tokens_after: 2_000,
        input_before: None,
        input_after: None,
        input_estimated: true,
    }
}

#[tokio::test]
async fn gather_facts_are_atomic_ordered_and_keep_one_canonical_commit() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("gather facts")?;
    let history = canonical_history(&store, &session.id, 2).await?;
    let first = batch(&session.id, 1, &history[..2], 1, 1, None);
    let written = event(&first, "z-first", ContextEventKind::EpisodeWritten);
    let mut bad = written.clone();
    bad.id = "bad".into();
    bad.trigger_text = "é".repeat(161);
    let before = store.storage_stats()?.chat_transaction_count;
    assert!(
        store
            .save_episode_batch_with_events(&first, &[written.clone(), bad])
            .is_err()
    );
    assert!(store.context_events(&session.id)?.is_empty());
    assert!(store.context_episodes(&session.id)?.is_empty());
    assert!(store.context_subject_chapters(&session.id)?.is_empty());
    assert_eq!(store.storage_stats()?.chat_transaction_count, before);
    store.save_episode_batch_with_events(&first, std::slice::from_ref(&written))?;
    assert_eq!(store.storage_stats()?.chat_transaction_count - before, 1);

    let mut next = batch(&session.id, 2, &history[2..], 1, 1, Some(&history[1].id));
    next.close_after_episode = Some(close(next.open_chapter.clone()));
    let mut next_written = event(&next, "b-second", ContextEventKind::EpisodeWritten);
    // Clock/ID sorting would reverse this event; insertion order is authoritative.
    next_written.created_at = "2026-10-26T00:00:00Z".into();
    let mut closed = event(&next, "a-third", ContextEventKind::ChapterClosed);
    closed.first_message_id = history[0].id.clone();
    closed.episode_ids.insert(0, first.episode.id.clone());
    closed.summary_id = Some(next.open_chapter.id.clone());
    store.save_episode_batch_with_events(&next, &[next_written.clone(), closed.clone()])?;
    assert_eq!(store.storage_stats()?.chat_transaction_count - before, 2);
    assert_eq!(
        store.context_events(&session.id)?,
        vec![written, next_written, closed]
    );
    let fork = store.fork(&session.id, "fork")?;
    assert!(store.context_events(&fork.id)?.is_empty());
    let child =
        store.create_session_with_parent("child facts", Default::default(), Some(&session.id))?;
    let mut child_history = history[..2].to_vec();
    for message in &mut child_history {
        message.session_id = child.id.clone();
    }
    store.append_batch(&child_history)?;
    let child_batch = batch(&child.id, 1, &child_history, 1, 1, None);
    let child_event = event(&child_batch, "child-fact", ContextEventKind::EpisodeWritten);
    store.save_episode_batch_with_events(&child_batch, &[child_event])?;
    store.delete_session(&child.id)?;
    let conn = Connection::open(store.chat_path(&session.id)?.unwrap())?;
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM context_events WHERE session_id=?1",
            [&child.id],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn apply_fact_uses_validation_transaction_and_survives_serving_wipe() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("apply facts")?;
    let history = canonical_history(&store, &session.id, 1).await?;
    let first = batch(&session.id, 1, &history, 1, 1, None);
    store.save_episode_batch(&first)?;
    let mut saved = view(
        &session.id,
        Some(store.message_seq(&session.id, &history[1].id)?),
        history[0].clone(),
    );
    saved.blocks = vec![ContextBlock {
        kind: ContextBlockKind::Opening,
        tokens: 20,
        id: None,
        first_message_id: Some(history[0].id.clone()),
        last_message_id: Some(history[0].id.clone()),
    }];
    let applied = event(&first, "applied", ContextEventKind::Applied);
    let mut bad = applied.clone();
    bad.last_message_id = "missing".into();
    let before = store.storage_stats()?.chat_transaction_count;
    assert!(store.save_context_view_with_event(&saved, &bad).is_err());
    assert!(store.context_events(&session.id)?.is_empty());
    assert!(store.context_view(&session.id)?.is_none());
    assert_eq!(store.storage_stats()?.chat_transaction_count, before);
    store.save_context_view_with_event(&saved, &applied)?;
    assert_eq!(store.storage_stats()?.chat_transaction_count - before, 1);
    assert_eq!(store.context_events(&session.id)?, vec![applied.clone()]);
    assert_eq!(
        store.context_view(&session.id)?.unwrap().blocks,
        saved.blocks
    );

    // Duplicate event insertion aborts the canonical view validation transaction.
    let mut replacement = saved.clone();
    replacement.system_prompt = "must not publish".into();
    assert!(
        store
            .save_context_view_with_event(&replacement, &applied)
            .is_err()
    );
    assert_eq!(
        store.context_view(&session.id)?.unwrap().system_prompt,
        saved.system_prompt
    );
    let mut invalid_view = saved.clone();
    invalid_view.frontier = Some(i64::MAX);
    let mut another = applied.clone();
    another.id = "another".into();
    assert!(
        store
            .save_context_view_with_event(&invalid_view, &another)
            .is_err()
    );
    assert_eq!(store.context_events(&session.id)?, vec![applied.clone()]);
    store.serving_wipe()?;
    assert_eq!(store.context_events(&session.id)?, vec![applied.clone()]);
    assert!(store.context_view(&session.id)?.is_none());
    drop(store);
    let store = Store::open_data_dir(dir.path())?;
    assert_eq!(store.context_events(&session.id)?, vec![applied]);
    Ok(())
}

#[tokio::test]
async fn context_event_references_and_legacy_view_defaults_are_checked() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("references")?;
    let history = canonical_history(&store, &session.id, 1).await?;
    let first = batch(&session.id, 1, &history, 1, 1, None);
    let valid = event(&first, "fact", ContextEventKind::EpisodeWritten);
    let mut variants = vec![valid.clone(); 7];
    variants[0].id.clear();
    variants[1].created_at.clear();
    variants[2].session_id = "foreign".into();
    variants[3].first_message_id = history[1].id.clone();
    variants[3].last_message_id = history[0].id.clone();
    variants[4].episode_ids = vec!["missing-episode".into()];
    variants[5].summary_id = Some("missing-summary".into());
    variants[6].kind = ContextEventKind::Applied;
    for bad in variants {
        assert!(
            store
                .save_episode_batch_with_events(&first, &[bad])
                .is_err()
        );
        assert!(store.context_events(&session.id)?.is_empty());
        assert!(store.context_episodes(&session.id)?.is_empty());
    }
    let saved = view(&session.id, None, history[0].clone());
    let mut json = serde_json::to_value(&saved)?;
    json.as_object_mut().unwrap().remove("blocks");
    json.as_object_mut().unwrap().remove("blocks_estimated");
    let old: ContextView = serde_json::from_value(json.clone())?;
    assert!(old.blocks.is_empty());
    assert!(!old.blocks_estimated);
    store.flush_serving()?;
    Connection::open(dir.path().join("serving.db"))?.execute(
        "INSERT INTO context_views(session_id,data) VALUES(?1,?2)",
        rusqlite::params![session.id, json.to_string()],
    )?;
    assert!(!store.context_view(&session.id)?.unwrap().blocks_estimated);
    Ok(())
}

#[tokio::test]
async fn schema_eleven_acquires_empty_facts_without_rewriting_history() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("v11")?;
    let history = canonical_history(&store, &session.id, 1).await?;
    let chat = store.chat_path(&session.id)?.unwrap();
    drop(store);
    let conn = Connection::open(&chat)?;
    let bodies: Vec<String> = conn
        .prepare("SELECT data FROM messages ORDER BY seq")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    conn.execute_batch("DROP TABLE context_events; DROP INDEX request_usage_message; ALTER TABLE request_usage DROP COLUMN message_seq; PRAGMA user_version=11; PRAGMA wal_checkpoint(TRUNCATE);")?;
    drop(conn);
    let store = Store::open_data_dir(dir.path())?;
    assert!(store.context_events(&session.id)?.is_empty());
    let tokens = store.reported_message_tokens(&session.id, 0..=i64::MAX)?;
    assert_eq!(tokens.len(), history.len());
    assert!(tokens.iter().all(|row| row.tokens.is_none()));
    let conn = Connection::open(&chat)?;
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?,
        12
    );
    let migrated: Vec<String> = conn
        .prepare("SELECT data FROM messages ORDER BY seq")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(bodies, migrated);
    drop(conn);
    drop(store);
    Connection::open(&chat)?.execute_batch("PRAGMA user_version=13;")?;
    // Startup inspects chat records while rebuilding its disposable catalog.
    assert!(Store::open_data_dir(dir.path()).is_err());
    assert_eq!(
        Connection::open(chat)?.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?,
        13
    );
    Ok(())
}
