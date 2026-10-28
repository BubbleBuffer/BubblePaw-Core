use super::*;

#[test]
fn rejected_single_file_notification_rolls_back_the_fact_and_its_gather() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("notification rollback")?;
    let first = Store::message(&session.id, Role::User, "question".into());
    let last = Store::message(&session.id, Role::Assistant, "answer".into());
    store.append_batch(&[first.clone(), last.clone()])?;
    let batch = EpisodeBatch {
        episode: ContextEpisode {
            id: "episode".into(),
            session_id: session.id.clone(),
            ordinal: 1,
            first_message_id: first.id.clone(),
            last_message_id: last.id.clone(),
            source_tokens: 100,
            cap: 20,
            origin_request_id: None,
            title: "Episode".into(),
            summary: "Retained facts".into(),
            model: "scripted".into(),
            usage: None,
            reasoning: None,
            created_at: now(),
            chapter_id: "chapter".into(),
        },
        open_chapter: ContextSubjectChapter {
            id: "chapter".into(),
            session_id: session.id.clone(),
            ordinal: 1,
            first_episode_ordinal: 1,
            last_episode_ordinal: 1,
            status: SubjectChapterStatus::Open,
            description: None,
            summary: None,
            related_ids: Vec::new(),
            related_evidence: Vec::new(),
            decisions: None,
            sections: None,
            model: None,
            usage: None,
            reasoning: None,
        },
        decisions: Vec::new(),
        closed_chapter: None,
        close_after_episode: None,
        expected_previous_coverage: None,
    };
    let fact = ContextEvent {
        id: "fact".into(),
        session_id: session.id.clone(),
        kind: ContextEventKind::EpisodeWritten,
        created_at: now(),
        trigger: crate::ContextTrigger::SourceSize,
        trigger_text: "Source target".into(),
        first_message_id: first.id,
        last_message_id: last.id,
        episode_ids: vec!["episode".into()],
        summary_id: Some("episode".into()),
        tokens_before: Some(100),
        tokens_after: 20,
        input_before: None,
        input_after: None,
        input_estimated: true,
    };
    store.read(|conn| {
        conn.execute_batch("CREATE TRIGGER reject_compaction_notification BEFORE INSERT ON events WHEN NEW.kind='context_compaction' BEGIN SELECT RAISE(ABORT,'notification rejected'); END;")?;
        Ok(())
    })?;
    let runtime = store.runtime();
    let mut receiver = runtime.log.subscribe();
    let cursor = runtime.log.last();
    let before = store.storage_stats()?.chat_transaction_count;
    assert!(
        store
            .save_episode_batch_with_events(&batch, std::slice::from_ref(&fact))
            .is_err()
    );
    assert_eq!(store.storage_stats()?.chat_transaction_count, before);
    assert!(store.context_events(&session.id)?.is_empty());
    assert!(store.context_episodes(&session.id)?.is_empty());
    assert!(store.context_subject_chapters(&session.id)?.is_empty());
    assert_eq!(runtime.log.last(), cursor);
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    store.read(|conn| {
        conn.execute_batch("DROP TRIGGER reject_compaction_notification;")?;
        Ok(())
    })?;
    store.save_episode_batch_with_events(&batch, &[fact])?;
    assert_eq!(store.storage_stats()?.chat_transaction_count - before, 1);
    let notification = receiver.try_recv()?;
    assert_eq!(notification.sequence, cursor + 1);
    assert!(notification.durable);
    assert!(matches!(
        notification.kind,
        crate::EventKind::ContextCompaction { .. }
    ));
    Ok(())
}
