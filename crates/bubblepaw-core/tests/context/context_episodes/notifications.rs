use super::*;
use bubblepaw_core::{EventKind, HarnessEvent};

fn assert_notification(
    notification: &HarnessEvent,
    fact: &ContextEvent,
    sequence: u64,
) -> Result<()> {
    assert!(notification.durable);
    assert_eq!(notification.sequence, sequence);
    assert_eq!(notification.session_id, fact.session_id);
    assert!(notification.run_id.is_empty());
    assert!(notification.origin.is_none());
    assert!(
        matches!(&notification.kind, EventKind::ContextCompaction { event_id, kind, trigger }
        if event_id == &fact.id && kind == &fact.kind && trigger == &fact.trigger)
    );
    let json = serde_json::to_value(notification)?;
    assert_eq!(json["type"], "context_compaction");
    assert_eq!(json["event_id"], fact.id);
    assert_eq!(json["kind"], serde_json::to_value(fact.kind)?);
    assert_eq!(json["trigger"], serde_json::to_value(fact.trigger)?);
    assert!(json.get("trigger_text").is_none());
    Ok(())
}

#[tokio::test]
async fn single_file_context_notifications_share_fact_commit_and_rollback_without_sequence_holes()
-> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("single-file notifications")?;
    let history = canonical_history(&store, &session.id, 2).await?;
    let agent = harness(&store);
    let mut receiver = agent.subscribe();
    let cursor = agent.last_sequence();
    let first = batch(&session.id, 1, &history[..2], 1, 1, None);
    let written = event(&first, "written", ContextEventKind::EpisodeWritten);
    let before = store.storage_stats()?;
    store.save_episode_batch_with_events(&first, std::slice::from_ref(&written))?;
    assert_eq!(
        store.storage_stats()?.chat_transaction_count - before.chat_transaction_count,
        1
    );
    assert_notification(&receiver.try_recv()?, &written, cursor + 1)?;

    let next = batch(&session.id, 2, &history[2..], 1, 1, Some(&history[1].id));
    let mut bad = event(&next, "bad", ContextEventKind::EpisodeWritten);
    bad.last_message_id = "missing".into();
    let before_failure = store.storage_stats()?.transaction_count;
    assert!(store.save_episode_batch_with_events(&next, &[bad]).is_err());
    assert_eq!(store.storage_stats()?.transaction_count, before_failure);
    assert_eq!(agent.last_sequence(), cursor + 1);
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    assert_eq!(store.context_events(&session.id)?, vec![written.clone()]);
    assert_eq!(store.context_episodes(&session.id)?.len(), 1);

    let next_written = event(&next, "next", ContextEventKind::EpisodeWritten);
    store.save_episode_batch_with_events(&next, std::slice::from_ref(&next_written))?;
    assert_notification(&receiver.try_recv()?, &next_written, cursor + 2)?;
    let before_read = store.storage_stats()?;
    let notifications = agent.events_after(cursor, Some(&session.id), 10)?;
    assert_eq!(notifications.len(), 2);
    assert_notification(&notifications[0], &written, cursor + 1)?;
    assert_notification(&notifications[1], &next_written, cursor + 2)?;
    assert_eq!(store.context_events(&session.id)?.len(), 2);
    let after_read = store.storage_stats()?;
    assert_eq!(after_read.transaction_count, before_read.transaction_count);
    assert_eq!(after_read.wal_bytes, before_read.wal_bytes);
    Ok(())
}

#[tokio::test]
async fn central_context_notifications_piggyback_one_existing_flush_and_reopen_in_order()
-> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("buffered notifications")?;
    let history = canonical_history(&store, &session.id, 2).await?;
    let agent = harness(&store);
    let mut receiver = agent.subscribe();
    let cursor = agent.last_sequence();
    store.flush_serving()?;
    let before = store.storage_stats()?;
    let first = batch(&session.id, 1, &history[..2], 1, 1, None);
    let written = event(&first, "first", ContextEventKind::EpisodeWritten);
    store.save_episode_batch_with_events(&first, std::slice::from_ref(&written))?;
    let mut next = batch(&session.id, 2, &history[2..], 1, 1, Some(&history[1].id));
    next.close_after_episode = Some(close(next.open_chapter.clone()));
    let next_written = event(&next, "second", ContextEventKind::EpisodeWritten);
    let mut closed = event(&next, "closed", ContextEventKind::ChapterClosed);
    closed.first_message_id = history[0].id.clone();
    closed.episode_ids.insert(0, first.episode.id.clone());
    closed.summary_id = Some(next.open_chapter.id.clone());
    closed.trigger = ContextTrigger::SubjectChange;
    store.save_episode_batch_with_events(&next, &[next_written.clone(), closed.clone()])?;
    let saved = view(
        &session.id,
        Some(store.message_seq(&session.id, &history[3].id)?),
        history[0].clone(),
    );
    let mut applied = event(&next, "applied", ContextEventKind::Applied);
    applied.trigger = ContextTrigger::Manual;
    store.save_context_view_with_event(&saved, &applied)?;
    let expected = vec![written, next_written, closed, applied];
    let buffered = store.storage_stats()?;
    assert_eq!(
        buffered.chat_transaction_count - before.chat_transaction_count,
        3
    );
    assert_eq!(
        buffered.serving_transaction_count,
        before.serving_transaction_count
    );
    assert_eq!(
        buffered.catalog_transaction_count,
        before.catalog_transaction_count
    );
    let external = Connection::open(dir.path().join("serving.db"))?;
    assert_eq!(
        external.query_row(
            "SELECT COUNT(*) FROM events WHERE seq>?1",
            [i64::try_from(cursor)?],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    let notifications = agent.events_after(cursor, Some(&session.id), 10)?;
    assert_eq!(notifications.len(), expected.len());
    for (index, fact) in expected.iter().enumerate() {
        let sequence = cursor + index as u64 + 1;
        assert_notification(&receiver.try_recv()?, fact, sequence)?;
        assert_notification(&notifications[index], fact, sequence)?;
    }
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    let before_read = store.storage_stats()?;
    assert_eq!(store.context_events(&session.id)?, expected);
    assert_eq!(agent.events_after(cursor, Some(&session.id), 10)?.len(), 4);
    assert_eq!(
        store.storage_stats()?.transaction_count,
        before_read.transaction_count
    );
    store.flush_serving()?;
    let flushed = store.storage_stats()?;
    assert_eq!(
        flushed.chat_transaction_count,
        buffered.chat_transaction_count
    );
    assert_eq!(
        flushed.serving_transaction_count - before.serving_transaction_count,
        1
    );
    assert_eq!(
        flushed.catalog_transaction_count,
        before.catalog_transaction_count
    );
    assert_eq!(
        external.query_row(
            "SELECT COUNT(*) FROM events WHERE seq>?1",
            [i64::try_from(cursor)?],
            |row| row.get::<_, i64>(0)
        )?,
        4
    );
    store.flush_serving()?;
    assert_eq!(
        store.storage_stats()?.serving_transaction_count,
        flushed.serving_transaction_count
    );
    drop(receiver);
    drop(agent);
    drop(external);
    drop(store);
    let reopened = Store::open_data_dir(dir.path())?;
    let agent = harness(&reopened);
    let notifications = agent.events_after(cursor, Some(&session.id), 10)?;
    assert_eq!(notifications.len(), expected.len());
    for (index, fact) in expected.iter().enumerate() {
        assert_notification(&notifications[index], fact, cursor + index as u64 + 1)?;
    }
    assert_eq!(reopened.context_events(&session.id)?, expected);
    Ok(())
}

#[tokio::test]
async fn rejected_central_apply_projection_writes_no_fact_or_notification() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("rejected projection")?;
    let history = canonical_history(&store, &session.id, 1).await?;
    let first = batch(&session.id, 1, &history, 1, 1, None);
    store.save_episode_batch(&first)?;
    let saved = view(
        &session.id,
        Some(store.message_seq(&session.id, &history[1].id)?),
        history[0].clone(),
    );
    store.save_context_view(&saved)?;
    let agent = harness(&store);
    let mut receiver = agent.subscribe();
    let cursor = agent.last_sequence();
    store.flush_serving()?;
    Connection::open(dir.path().join("serving.db"))?.execute_batch(
        "CREATE TRIGGER reject_view_projection BEFORE UPDATE ON context_views BEGIN SELECT RAISE(ABORT,'view projection rejected'); END;",
    )?;
    let mut replacement = saved.clone();
    replacement.system_prompt = "must not become visible".into();
    let applied = event(&first, "projection-rejected", ContextEventKind::Applied);
    let before = store.storage_stats()?;
    assert!(
        store
            .save_context_view_with_event(&replacement, &applied)
            .is_err()
    );
    assert!(store.context_events(&session.id)?.is_empty());
    assert_eq!(
        store.context_view(&session.id)?.unwrap().system_prompt,
        saved.system_prompt
    );
    assert_eq!(agent.last_sequence(), cursor);
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    assert!(
        agent
            .events_after(cursor, Some(&session.id), 10)?
            .is_empty()
    );
    assert_eq!(
        store.storage_stats()?.chat_transaction_count,
        before.chat_transaction_count
    );
    store.flush_serving()?;
    assert_eq!(
        store.storage_stats()?.serving_transaction_count,
        before.serving_transaction_count
    );
    Ok(())
}

#[tokio::test]
async fn canonical_commit_failure_restores_only_the_staged_view_without_publishing_apply()
-> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("failed canonical commit")?;
    let history = canonical_history(&store, &session.id, 1).await?;
    let first = batch(&session.id, 1, &history, 1, 1, None);
    store.save_episode_batch(&first)?;
    let saved = view(
        &session.id,
        Some(store.message_seq(&session.id, &history[1].id)?),
        history[0].clone(),
    );
    store.save_context_view(&saved)?;
    let agent = harness(&store);
    let mut receiver = agent.subscribe();
    let cursor = agent.last_sequence();
    store.flush_serving()?;
    let canonical = Connection::open(store.chat_path(&session.id)?.unwrap())?;
    canonical.execute_batch("CREATE TABLE apply_commit_guard(parent TEXT REFERENCES sessions(id) DEFERRABLE INITIALLY DEFERRED);
        CREATE TRIGGER reject_apply_commit AFTER INSERT ON context_events BEGIN INSERT INTO apply_commit_guard VALUES('missing-session'); END;")?;
    let mut replacement = saved.clone();
    replacement.system_prompt = "staged before failed commit".into();
    let applied = event(&first, "commit-rejected", ContextEventKind::Applied);
    let before = store.storage_stats()?;
    let error = store
        .save_context_view_with_event(&replacement, &applied)
        .unwrap_err();
    assert!(format!("{error:#}").contains("FOREIGN KEY"));
    assert!(store.context_events(&session.id)?.is_empty());
    assert_eq!(
        store.context_view(&session.id)?.unwrap().system_prompt,
        saved.system_prompt
    );
    assert_eq!(agent.last_sequence(), cursor);
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    assert_eq!(
        store.storage_stats()?.chat_transaction_count,
        before.chat_transaction_count
    );
    canonical.execute_batch("DROP TRIGGER reject_apply_commit;")?;
    // Retry uses the same fact ID: neither the rejected fact nor a stream sequence survived.
    store.save_context_view_with_event(&replacement, &applied)?;
    assert_notification(&receiver.try_recv()?, &applied, cursor + 1)?;
    assert_eq!(
        store.context_view(&session.id)?.unwrap().system_prompt,
        replacement.system_prompt
    );
    assert_eq!(
        store.storage_stats()?.chat_transaction_count - before.chat_transaction_count,
        1
    );
    store.flush_serving()?;
    assert_eq!(
        store.storage_stats()?.serving_transaction_count - before.serving_transaction_count,
        1
    );
    Ok(())
}

#[tokio::test]
async fn central_apply_journal_can_reread_the_new_view_before_broadcast() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("visible apply projection")?;
    let history = canonical_history(&store, &session.id, 1).await?;
    let first = batch(&session.id, 1, &history, 1, 1, None);
    store.save_episode_batch(&first)?;
    let saved = view(
        &session.id,
        Some(store.message_seq(&session.id, &history[1].id)?),
        history[0].clone(),
    );
    store.save_context_view(&saved)?;
    let agent = harness(&store);
    let mut receiver = agent.subscribe();
    let cursor = agent.last_sequence();
    store.flush_serving()?;
    let unchanged = event(&first, "unchanged", ContextEventKind::Applied);
    assert!(
        store
            .save_context_view_with_event(&saved, &unchanged)
            .is_err()
    );
    assert!(store.context_events(&session.id)?.is_empty());
    assert_eq!(agent.last_sequence(), cursor);
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    Connection::open(dir.path().join("serving.db"))?.execute_batch(
        "CREATE TRIGGER require_applied_projection BEFORE INSERT ON events
         WHEN NEW.kind='context_compaction' AND COALESCE(json_extract((SELECT data FROM context_views WHERE session_id=NEW.session_id),'$.system_prompt'),'')!='new visible instructions'
         BEGIN SELECT RAISE(ABORT,'notification preceded its view'); END;",
    )?;
    let mut replacement = saved.clone();
    replacement.system_prompt = "new visible instructions".into();
    let applied = event(&first, "visible", ContextEventKind::Applied);
    store.save_context_view_with_event(&replacement, &applied)?;
    assert_notification(&receiver.try_recv()?, &applied, cursor + 1)?;
    assert_eq!(
        store.context_view(&session.id)?.unwrap().system_prompt,
        replacement.system_prompt
    );
    Ok(())
}
