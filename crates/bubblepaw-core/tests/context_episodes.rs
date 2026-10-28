#[path = "context_episodes/facts.rs"]
mod facts;
#[path = "context_episodes/legacy_metadata.rs"]
mod legacy_metadata;
mod support;
use anyhow::Result;
use bubblepaw_core::{
    ContextChapter, ContextCheckpoint, ContextDecision, ContextEpisode, ContextSubjectChapter,
    ContextView, DecisionKind, EpisodeBatch, InputKind, Message, MessageOrigin, RelatedEvidence,
    Role, RunOptions, Store, SubjectChapterStatus, Usage, is_typed_user,
};
use support::harness;
#[path = "context_episodes/fixtures.rs"]
mod fixtures;
use fixtures::{batch, canonical_history, close, persisted_context, view};

#[tokio::test]
async fn context_views_replace_atomically_freeze_restart_and_start_empty_on_fork() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("views.sqlite");
    let store = Store::open(&path)?;
    let session = store.create_session("views")?;
    let history = canonical_history(&store, &session.id, 3).await?;
    let canonical = serde_json::to_value(&history)?;
    store.save_episode_batch(&batch(&session.id, 1, &history[..2], 1, 1, None))?;
    store.save_episode_batch(&batch(
        &session.id,
        2,
        &history[2..4],
        1,
        1,
        Some(&history[1].id),
    ))?;
    let seq = store.message_seq(&session.id, &history[1].id)?;
    assert_eq!(
        store.context_view_frontier_message_id(&session.id, Some(seq))?,
        Some(history[1].id.clone())
    );
    assert!(
        store
            .context_view_frontier_message_id(&session.id, None)?
            .is_none()
    );
    let mut saved = view(&session.id, Some(seq), history[0].clone());
    assert!(store.context_view(&session.id)?.is_none());
    let before = store.storage_stats()?.serving_transaction_count;
    store.save_context_view(&saved)?;
    assert_eq!(store.storage_stats()?.serving_transaction_count, before);
    assert_eq!(
        serde_json::to_value(store.context_view(&session.id)?.unwrap())?,
        serde_json::to_value(&saved)?
    );
    store.flush_serving()?;
    assert_eq!(store.storage_stats()?.serving_transaction_count, before + 1);
    let conn = rusqlite::Connection::open(Store::legacy_data_dir(&path).join("serving.db"))?;
    let data: String = conn.query_row(
        "SELECT data FROM context_views WHERE session_id=?1",
        [&session.id],
        |row| row.get(0),
    )?;
    assert!(!data.contains(&history[0].content));
    drop(conn);
    let baseline = serde_json::to_value(&saved)?;
    for _ in 0..4 {
        assert_eq!(
            serde_json::to_value(store.context_view(&session.id)?.unwrap())?,
            baseline
        );
    }
    assert_eq!(store.storage_stats()?.serving_transaction_count, before + 1);
    for invalid in [
        Some(store.message_seq(&session.id, &history[0].id)?),
        Some(i64::MAX),
    ] {
        let mut bad = saved.clone();
        bad.frontier = invalid;
        assert!(store.save_context_view(&bad).is_err());
        assert_eq!(
            serde_json::to_value(store.context_view(&session.id)?.unwrap())?,
            baseline
        );
        assert_eq!(store.storage_stats()?.serving_transaction_count, before + 1);
    }
    for bad in [
        ContextView {
            session_id: "missing".into(),
            ..saved.clone()
        },
        ContextView {
            masked_tool_output_ids: vec![history[4].id.clone()],
            ..saved.clone()
        },
        ContextView {
            clipped_tool_call_ids: vec!["missing".into()],
            ..saved.clone()
        },
    ] {
        assert!(store.save_context_view(&bad).is_err());
        assert_eq!(
            serde_json::to_value(store.context_view(&session.id)?.unwrap())?,
            baseline
        );
    }
    // Unknown-session lookup can refresh the disposable catalog projection.
    // Flush before an external connection installs its failure trigger.
    store.flush_serving()?;
    let before = store.storage_stats()?.serving_transaction_count;
    saved.frontier = Some(store.message_seq(&session.id, &history[3].id)?);
    saved.prefix[0].content = "replacement summary".into();
    saved.system_prompt = "Replacement instructions".into();
    saved.created_at = "2026-10-08T01:00:00Z".into();
    saved.tokens = 30_000;
    let conn = rusqlite::Connection::open(Store::legacy_data_dir(&path).join("serving.db"))?;
    conn.execute_batch("CREATE TRIGGER reject_view_replacement BEFORE UPDATE ON context_views BEGIN SELECT RAISE(ABORT,'scripted replacement failure'); END;")?;
    assert!(store.save_context_view(&saved).is_err());
    assert_eq!(
        serde_json::to_value(store.context_view(&session.id)?.unwrap())?,
        baseline
    );
    assert_eq!(store.storage_stats()?.serving_transaction_count, before);
    conn.execute_batch("DROP TRIGGER reject_view_replacement;")?;
    drop(conn);
    store.save_context_view(&saved)?;
    assert_eq!(store.storage_stats()?.serving_transaction_count, before);
    store.flush_serving()?;
    assert_eq!(store.storage_stats()?.serving_transaction_count, before + 1);
    assert_eq!(
        serde_json::to_value(store.messages(&session.id)?)?,
        canonical
    );
    harness(&store)
        .prompt(
            &session.id,
            "append after compaction",
            RunOptions::default(),
        )
        .await?;
    assert_eq!(
        serde_json::to_value(store.context_view(&session.id)?.unwrap())?,
        serde_json::to_value(&saved)?
    );
    let fork = store.fork(&session.id, "fork")?;
    assert!(store.context_view(&fork.id)?.is_none());
    assert!(store.context_checkpoint(&fork.id)?.is_none());
    assert!(store.context_chapters(&fork.id)?.is_empty());
    assert!(store.context_episodes(&fork.id)?.is_empty());
    assert!(store.context_subject_chapters(&fork.id)?.is_empty());
    assert!(store.context_decisions(&fork.id)?.is_empty());
    drop(store);
    let reopened = Store::open(&path)?;
    assert_eq!(
        serde_json::to_value(reopened.context_view(&session.id)?.unwrap())?,
        serde_json::to_value(saved)?
    );
    let conn = rusqlite::Connection::open(Store::legacy_data_dir(&path).join("serving.db"))?;
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM context_views", [], |row| row
            .get::<_, i64>(0))?,
        1
    );
    Ok(())
}

#[tokio::test]
async fn context_views_migrate_v3_v4_v5_and_reopen_v6_without_changing_history() -> Result<()> {
    for version in [3, 4, 5, 6] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("migration.sqlite");
        let store = Store::open(&path)?;
        let session = store.create_session("migration")?;
        let history = canonical_history(&store, &session.id, 3).await?;
        let checkpoint = ContextCheckpoint {
            files_touched: Vec::new(),
            id: "legacy-checkpoint".into(),
            session_id: session.id.clone(),
            through_message_id: history[1].id.clone(),
            model: "legacy".into(),
            summary: "legacy checkpoint text".into(),
            created_at: history[1].created_at.clone(),
            usage: None,
        };
        store.save_context_checkpoint(&checkpoint)?;
        if version >= 4 {
            store.save_context_chapter(&ContextChapter {
                id: "legacy-chapter".into(),
                session_id: session.id.clone(),
                ordinal: 1,
                first_message_id: history[2].id.clone(),
                last_message_id: history[3].id.clone(),
                title: "Legacy".into(),
                summary: "legacy chapter text".into(),
                model: "legacy".into(),
                usage: None,
                created_at: history[3].created_at.clone(),
            })?;
        }
        if version >= 5 {
            store.save_episode_batch(&batch(
                &session.id,
                1,
                &history[4..],
                1,
                1,
                Some(&history[3].id),
            ))?;
        }
        let canonical = serde_json::to_value(&history)?;
        let legacy = serde_json::to_value(store.context_checkpoint(&session.id)?)?;
        let chapters = serde_json::to_value(store.context_chapters(&session.id)?)?;
        let episodes = serde_json::to_value(store.context_episodes(&session.id)?)?;
        let boundary = if version == 3 {
            &history[1]
        } else if version == 4 {
            &history[3]
        } else {
            &history[5]
        };
        let saved = view(
            &session.id,
            Some(store.message_seq(&session.id, &boundary.id)?),
            history[0].clone(),
        );
        if version == 6 {
            store.save_context_view(&saved)?;
        }
        drop(store);
        support::replace_with_legacy_fixture(&path, version)?;
        let store = Store::open(&path)?;
        assert_eq!(store.storage_stats()?.chat_transaction_count, 0);
        assert_eq!(
            serde_json::to_value(store.messages(&session.id)?)?,
            canonical
        );
        assert_eq!(
            serde_json::to_value(store.context_checkpoint(&session.id)?)?,
            legacy
        );
        assert_eq!(
            serde_json::to_value(store.context_chapters(&session.id)?)?,
            chapters
        );
        assert_eq!(
            serde_json::to_value(store.context_episodes(&session.id)?)?,
            episodes
        );
        if version == 6 {
            assert_eq!(
                serde_json::to_value(store.context_view(&session.id)?.unwrap())?,
                serde_json::to_value(&saved)?
            );
        } else {
            assert!(store.context_view(&session.id)?.is_none());
            store.save_context_view(&saved)?;
            assert_eq!(store.storage_stats()?.chat_transaction_count, 1);
        }
        let conn = rusqlite::Connection::open(store.chat_path(&session.id)?.unwrap())?;
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?,
            12
        );
    }
    Ok(())
}

#[tokio::test]
async fn scripted_two_hundred_episodes_and_twenty_views_have_bounded_storage() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("storage.sqlite");
    let store = Store::open(&path)?;
    let session = store.create_session("storage")?;
    let history = canonical_history(&store, &session.id, 200).await?;
    let canonical = serde_json::to_value(&history)?;
    let before = store.storage_stats()?.chat_transaction_count;
    for ordinal in 1..=200 {
        let start = (ordinal - 1) * 2;
        let mut next = batch(
            &session.id,
            ordinal,
            &history[start..start + 2],
            1,
            1,
            start.checked_sub(1).map(|index| history[index].id.as_str()),
        );
        next.episode.summary = "S".repeat(8_000);
        store.save_episode_batch(&next)?;
        if ordinal % 10 == 0 {
            let mut prefix = history[0].clone();
            prefix.content = "P".repeat(96_000);
            let saved = view(
                &session.id,
                Some(store.message_seq(&session.id, &history[start + 1].id)?),
                prefix,
            );
            store.save_context_view(&saved)?;
        }
    }
    let stats = store.storage_stats()?;
    eprintln!(
        "200 scripted episodes + 20 view replacements: database={} bytes, pages={} bytes, WAL={} bytes, context transactions={}",
        stats.database_bytes,
        stats.page_bytes,
        stats.wal_bytes,
        stats.chat_transaction_count - before
    );
    assert_eq!(stats.chat_transaction_count - before, 220);
    assert_eq!(store.context_episodes(&session.id)?.len(), 200);
    assert_eq!(store.context_decisions(&session.id)?.len(), 200);
    assert_eq!(
        serde_json::to_value(store.messages(&session.id)?)?,
        canonical
    );
    // Includes canonical history, 1.6 MB of summaries, and one 96 KB prefix.
    assert!(stats.page_bytes < 6 * 1024 * 1024);
    assert!(stats.database_bytes < 6 * 1024 * 1024);
    for database in [
        store.chat_path(&session.id)?.unwrap(),
        Store::legacy_data_dir(&path).join("serving.db"),
        Store::legacy_data_dir(&path).join("catalog.db"),
    ] {
        let wal = std::path::PathBuf::from(format!("{}-wal", database.display()));
        let bytes = std::fs::metadata(&wal).map_or(0, |metadata| metadata.len());
        assert!(
            bytes <= 1024 * 1024,
            "retained WAL {}: {bytes} bytes",
            wal.display()
        );
    }
    store.idle_checkpoint()?;
    assert_eq!(store.storage_stats()?.wal_bytes, 0);
    let conn = rusqlite::Connection::open(Store::legacy_data_dir(&path).join("serving.db"))?;
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM context_views", [], |row| row
            .get::<_, i64>(0))?,
        1
    );
    Ok(())
}

#[tokio::test]
async fn origin_round_trips_admission_queue_restart_and_consumption() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("origins.sqlite");
    let store = Store::open(&path)?;
    let session = store.create_session("origin")?;
    let typed = store.queue(&session.id, InputKind::Followup, "typed")?;
    let generated = store.queue_with_origin(
        &session.id,
        InputKind::Steer,
        "generated",
        MessageOrigin::Generated,
    )?;
    let child = store.queue_with_origin(
        &session.id,
        InputKind::Followup,
        "Task:\nchild",
        MessageOrigin::ChildTask,
    )?;
    assert_eq!(typed.origin, Some(MessageOrigin::Typed));
    assert_eq!(generated.origin, Some(MessageOrigin::Generated));
    assert_eq!(child.origin, Some(MessageOrigin::ChildTask));
    assert_eq!(
        serde_json::to_value(store.snapshot(&session.id, 0, 0, 10)?.pending_inputs)?,
        serde_json::to_value(store.queued_inputs(&session.id)?)?
    );
    drop(store);
    let store = Store::open(&path)?;
    assert_eq!(store.queued_inputs(&session.id)?[2].origin, child.origin);
    let agent = harness(&store);
    agent.resume(&session.id, RunOptions::default()).await?;
    let messages = store.messages(&session.id)?;
    let users: Vec<_> = messages.iter().filter(|m| m.role == Role::User).collect();
    assert_eq!(users.len(), 3);
    assert_eq!(users[0].origin, typed.origin);
    assert_eq!(users[1].origin, generated.origin);
    assert_eq!(users[2].origin, child.origin);
    assert!(is_typed_user(users[0]));
    assert!(!is_typed_user(users[1]));
    assert!(!is_typed_user(users[2]));
    for queued in store.queued_inputs(&session.id)? {
        assert_eq!(queued.state, "consumed");
        assert!(queued.text.is_empty());
        assert!(queued.origin.is_some());
    }
    agent
        .prompt(
            &session.id,
            "Task:\nchild admitted",
            RunOptions {
                origin: MessageOrigin::ChildTask,
                ..Default::default()
            },
        )
        .await?;
    let admitted = store.messages(&session.id)?;
    assert_eq!(
        admitted[admitted.len() - 2].origin,
        Some(MessageOrigin::ChildTask)
    );
    assert!(!is_typed_user(&admitted[admitted.len() - 2]));
    assert_eq!(admitted.last().unwrap().origin, None);
    Ok(())
}

#[tokio::test]
async fn origin_legacy_fallback_excludes_only_generated_wrappers() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("legacy")?;
    let mut message = canonical_history(&store, &session.id, 1).await?.remove(0);
    message.origin = None;
    let json = serde_json::to_value(&message)?;
    assert!(json.get("origin").is_none());
    let restored: Message = serde_json::from_value(json)?;
    assert!(is_typed_user(&restored));
    for wrapper in ["Task:\nchild", "Explicit context (JSON):\n{}"] {
        message.content = wrapper.into();
        assert!(!is_typed_user(&message));
        message.origin = Some(MessageOrigin::Typed);
        assert!(is_typed_user(&message));
        message.origin = None;
    }
    message.content = "Task: a normal user sentence".into();
    assert!(is_typed_user(&message));
    message.role = Role::Assistant;
    message.origin = Some(MessageOrigin::Typed);
    assert!(!is_typed_user(&message));
    Ok(())
}

#[tokio::test]
async fn batches_extend_close_restart_and_fork_in_one_transaction_each() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("episodes.sqlite");
    let store = Store::open(&path)?;
    let session = store.create_session("source")?;
    let history = canonical_history(&store, &session.id, 4).await?;
    let canonical = serde_json::to_value(&history)?;
    let before = store.storage_stats()?.chat_transaction_count;
    let first = batch(&session.id, 1, &history[..2], 1, 1, None);
    store.save_episode_batch(&first)?;
    assert_eq!(store.storage_stats()?.chat_transaction_count, before + 1);
    let second = batch(&session.id, 2, &history[2..4], 1, 1, Some(&history[1].id));
    store.save_episode_batch(&second)?;
    assert_eq!(store.storage_stats()?.chat_transaction_count, before + 2);
    assert_eq!(
        store
            .open_subject_chapter(&session.id)?
            .unwrap()
            .last_episode_ordinal,
        2
    );
    let mut third = batch(&session.id, 3, &history[4..6], 2, 3, Some(&history[3].id));
    third.closed_chapter = Some(close(second.open_chapter.clone()));
    store.save_episode_batch(&third)?;
    assert_eq!(store.storage_stats()?.chat_transaction_count, before + 3);
    let chapters = store.context_subject_chapters(&session.id)?;
    assert_eq!(chapters.len(), 2);
    assert_eq!(chapters[0].status, SubjectChapterStatus::Closed);
    assert_eq!(chapters[0].first_episode_ordinal, 1);
    assert_eq!(chapters[0].last_episode_ordinal, 2);
    assert_eq!(chapters[1].status, SubjectChapterStatus::Open);
    assert_eq!(chapters[1].first_episode_ordinal, 3);
    assert_eq!(store.context_decisions(&session.id)?.len(), 3);
    assert_eq!(
        store.context_episodes(&session.id)?[1].chapter_id,
        first.open_chapter.id
    );
    let persisted = persisted_context(&store, &session.id)?;
    for _ in 0..3 {
        assert_eq!(persisted_context(&store, &session.id)?, persisted);
        store.open_subject_chapter(&session.id)?;
        assert_eq!(
            store.episode_coverage_token(&session.id)?,
            Some(history[5].id.clone())
        );
    }
    assert_eq!(store.storage_stats()?.chat_transaction_count, before + 3);
    assert_eq!(
        serde_json::to_value(store.messages(&session.id)?)?,
        canonical
    );
    let fork = store.fork(&session.id, "fork")?;
    assert!(store.context_episodes(&fork.id)?.is_empty());
    assert!(store.context_subject_chapters(&fork.id)?.is_empty());
    assert!(store.context_decisions(&fork.id)?.is_empty());
    assert!(store.open_subject_chapter(&fork.id)?.is_none());
    assert!(store.episode_coverage_token(&fork.id)?.is_none());
    let fork_history = store.messages(&fork.id)?;
    assert_eq!(fork_history.len(), history.len());
    for (source, copied) in history.iter().zip(&fork_history) {
        assert_ne!(source.id, copied.id);
        assert_eq!(source.origin, copied.origin);
        assert_eq!(source.content, copied.content);
    }
    drop(store);
    let reopened = Store::open(&path)?;
    assert_eq!(reopened.storage_stats()?.chat_transaction_count, 0);
    assert_eq!(persisted_context(&reopened, &session.id)?, persisted);
    assert_eq!(
        serde_json::to_value(reopened.messages(&session.id)?)?,
        canonical
    );
    Ok(())
}

#[tokio::test]
async fn invalid_coverage_membership_endpoints_and_origin_are_atomic() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("source")?;
    let history = canonical_history(&store, &session.id, 3).await?;
    let foreign = store.create_session("foreign endpoint")?;
    harness(&store)
        .prompt(&foreign.id, "other", RunOptions::default())
        .await?;
    let foreign_history = store.messages(&foreign.id)?;
    let agent = harness(&store);
    agent
        .prompt(
            &session.id,
            "generated child request",
            RunOptions {
                origin: MessageOrigin::ChildTask,
                ..Default::default()
            },
        )
        .await?;
    let all_history = store.messages(&session.id)?;
    let generated_id = &all_history[all_history.len() - 2].id;
    let first = batch(&session.id, 1, &history[..2], 1, 1, None);
    store.save_episode_batch(&first)?;
    let next = batch(&session.id, 2, &history[2..4], 1, 1, Some(&history[1].id));
    let baseline = persisted_context(&store, &session.id)?;
    let before = store.storage_stats()?.chat_transaction_count;
    let reject = |bad: &EpisodeBatch| -> Result<()> {
        assert!(
            store.save_episode_batch(bad).is_err(),
            "accepted invalid batch {bad:?}"
        );
        assert_eq!(persisted_context(&store, &session.id)?, baseline);
        assert_eq!(store.storage_stats()?.chat_transaction_count, before);
        Ok(())
    };
    for expected in [None, Some(history[0].id.clone()), Some("missing".into())] {
        let mut bad = next.clone();
        bad.expected_previous_coverage = expected;
        reject(&bad)?;
    }
    for first_id in [&history[0].id, &history[3].id, &foreign_history[0].id] {
        let mut bad = next.clone();
        bad.episode.first_message_id = first_id.clone();
        reject(&bad)?;
    }
    for last_id in [&history[1].id, &foreign_history[1].id, &"missing".into()] {
        let mut bad = next.clone();
        bad.episode.last_message_id = last_id.clone();
        reject(&bad)?;
    }
    for ordinal in [0, 1, 3, usize::MAX] {
        let mut bad = next.clone();
        bad.episode.ordinal = ordinal;
        reject(&bad)?;
    }
    let mut bad = next.clone();
    bad.episode.id = first.episode.id.clone();
    reject(&bad)?;
    bad = next.clone();
    bad.open_chapter.first_episode_ordinal = 2;
    reject(&bad)?;
    bad = next.clone();
    bad.open_chapter.id = "different".into();
    bad.episode.chapter_id = "different".into();
    reject(&bad)?;
    bad = next.clone();
    bad.open_chapter.summary = Some("premature".into());
    reject(&bad)?;
    bad = next.clone();
    bad.episode.origin_request_id = Some(history[4].id.clone());
    reject(&bad)?;
    bad = next.clone();
    bad.episode.origin_request_id = Some(history[1].id.clone());
    reject(&bad)?;
    bad = next.clone();
    bad.episode.origin_request_id = Some(foreign_history[0].id.clone());
    reject(&bad)?;
    bad = next.clone();
    bad.episode.summary = String::new();
    reject(&bad)?;
    for title in [" ", "first\nsecond"] {
        bad = next.clone();
        bad.episode.title = title.into();
        reject(&bad)?;
    }
    for option in [
        &history[0].id,
        &history[3].id,
        &foreign_history[0].id,
        generated_id,
    ] {
        bad = next.clone();
        bad.episode.last_message_id = generated_id.clone();
        bad.decisions[0].option_message_ids = vec![option.clone()];
        bad.decisions[0].probabilities = vec![0.0, 1.0];
        reject(&bad)?;
    }
    for probabilities in [
        vec![1.0],
        vec![0.2, 0.2],
        vec![f64::NAN, 1.0],
        vec![f64::INFINITY, 0.0],
        vec![-0.1, 1.1],
    ] {
        bad = next.clone();
        bad.decisions[0].probabilities = probabilities;
        reject(&bad)?;
    }
    bad = next.clone();
    bad.decisions[0].choice = Some(history[0].id.clone());
    reject(&bad)?;
    bad = next.clone();
    bad.decisions[0].session_id = foreign.id.clone();
    reject(&bad)?;
    bad = next.clone();
    bad.decisions[0].episode_ordinal = 1;
    reject(&bad)?;
    let mut continuation = next;
    continuation.episode.origin_request_id = Some(history[0].id.clone());
    // The cap is a target: a valid summary can exceed it.
    continuation.episode.cap = 1;
    store.save_episode_batch(&continuation)?;
    assert_eq!(store.storage_stats()?.chat_transaction_count, before + 1);
    let checkpoint = ContextCheckpoint {
        files_touched: Vec::new(),
        id: "cannot advance".into(),
        session_id: session.id.clone(),
        through_message_id: history[3].id.clone(),
        model: "legacy".into(),
        summary: "legacy".into(),
        created_at: history[3].created_at.clone(),
        usage: None,
    };
    assert!(store.save_context_checkpoint(&checkpoint).is_err());
    Ok(())
}

#[tokio::test]
async fn split_decision_preserves_offered_options_and_choice_in_uncovered_remainder() -> Result<()>
{
    let store = Store::in_memory()?;
    let session = store.create_session("split")?;
    let history = canonical_history(&store, &session.id, 2).await?;
    // The question was asked over both turns, then its answer cut the saved
    // episode before the second typed request. Keep that original question.
    let mut prepiece = batch(&session.id, 1, &history[..2], 1, 1, None);
    prepiece.episode.source_tokens = 100;
    prepiece.episode.cap = 6;
    prepiece.decisions[0].option_message_ids = vec![history[0].id.clone(), history[2].id.clone()];
    prepiece.decisions[0].probabilities = vec![0.10, 0.85, 0.05];
    prepiece.decisions[0].choice = Some(history[2].id.clone());
    prepiece.close_after_episode = Some(close(prepiece.open_chapter.clone()));
    let before = store.storage_stats()?.chat_transaction_count;
    store.save_episode_batch(&prepiece)?;
    assert_eq!(store.storage_stats()?.chat_transaction_count, before + 1);
    assert!(store.open_subject_chapter(&session.id)?.is_none());
    let decisions = store.context_decisions(&session.id)?;
    assert_eq!(
        serde_json::to_value(&decisions[0])?,
        serde_json::to_value(&prepiece.decisions[0])?
    );
    assert_eq!(
        store.context_episodes(&session.id)?[0].last_message_id,
        history[1].id
    );
    assert_eq!(
        store.episode_coverage_token(&session.id)?,
        Some(history[1].id.clone())
    );
    let remainder = batch(&session.id, 2, &history[2..4], 2, 2, Some(&history[1].id));
    store.save_episode_batch(&remainder)?;
    assert_eq!(store.storage_stats()?.chat_transaction_count, before + 2);
    assert_eq!(
        store.context_subject_chapters(&session.id)?[0].status,
        SubjectChapterStatus::Closed
    );
    assert_eq!(
        store.context_episodes(&session.id)?[1].first_message_id,
        history[2].id
    );
    assert_eq!(
        store.context_decisions(&session.id)?[0].choice,
        Some(history[2].id.clone())
    );
    Ok(())
}

#[tokio::test]
async fn mid_batch_sql_failure_rolls_back_episode_decisions_and_chapter_changes() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("atomic")?;
    let history = canonical_history(&store, &session.id, 2).await?;
    let mut first = batch(&session.id, 1, &history[..2], 1, 1, None);
    first.decisions.push(first.decisions[0].clone());
    let before = store.storage_stats()?.chat_transaction_count;
    let error = store.save_episode_batch(&first).unwrap_err();
    assert!(error.to_string().contains("UNIQUE constraint failed"));
    assert!(store.context_episodes(&session.id)?.is_empty());
    assert!(store.context_subject_chapters(&session.id)?.is_empty());
    assert!(store.context_decisions(&session.id)?.is_empty());
    assert_eq!(store.storage_stats()?.chat_transaction_count, before);
    first.decisions.pop();
    store.save_episode_batch(&first)?;
    let baseline = persisted_context(&store, &session.id)?;
    let mut second = batch(&session.id, 2, &history[2..4], 2, 2, Some(&history[1].id));
    second.closed_chapter = Some(close(first.open_chapter));
    second.decisions.push(second.decisions[0].clone());
    let before = store.storage_stats()?.chat_transaction_count;
    assert!(store.save_episode_batch(&second).is_err());
    assert_eq!(persisted_context(&store, &session.id)?, baseline);
    assert_eq!(
        store.open_subject_chapter(&session.id)?.unwrap().status,
        SubjectChapterStatus::Open
    );
    assert_eq!(store.storage_stats()?.chat_transaction_count, before);
    Ok(())
}

#[tokio::test]
async fn related_links_are_backward_same_session_and_closed_membership_stays_frozen() -> Result<()>
{
    let store = Store::in_memory()?;
    let session = store.create_session("subjects")?;
    let history = canonical_history(&store, &session.id, 4).await?;
    let foreign = store.create_session("foreign subject")?;
    let foreign_history = canonical_history(&store, &foreign.id, 2).await?;
    let foreign_batch = batch(&foreign.id, 1, &foreign_history[..2], 1, 1, None);
    store.save_episode_batch(&foreign_batch)?;
    let mut foreign_next = batch(
        &foreign.id,
        2,
        &foreign_history[2..4],
        2,
        2,
        Some(&foreign_history[1].id),
    );
    foreign_next.closed_chapter = Some(close(foreign_batch.open_chapter.clone()));
    store.save_episode_batch(&foreign_next)?;
    let first = batch(&session.id, 1, &history[..2], 1, 1, None);
    store.save_episode_batch(&first)?;
    let mut second = batch(&session.id, 2, &history[2..4], 2, 2, Some(&history[1].id));
    second.closed_chapter = Some(close(first.open_chapter.clone()));
    store.save_episode_batch(&second)?;
    let mut third = batch(&session.id, 3, &history[4..6], 3, 3, Some(&history[3].id));
    third.closed_chapter = Some(close(second.open_chapter.clone()));
    let baseline = persisted_context(&store, &session.id)?;
    let before = store.storage_stats()?.chat_transaction_count;
    for related in [
        second.open_chapter.id.as_str(),
        third.open_chapter.id.as_str(),
        foreign_batch.open_chapter.id.as_str(),
        "missing",
    ] {
        let mut bad = third.clone();
        let closing = bad.closed_chapter.as_mut().unwrap();
        closing.related_ids = vec![related.into()];
        closing.related_evidence = vec!["return to my earlier goal".into()];
        assert!(store.save_episode_batch(&bad).is_err());
        assert_eq!(persisted_context(&store, &session.id)?, baseline);
        assert_eq!(store.storage_stats()?.chat_transaction_count, before);
    }
    let closed = third.closed_chapter.as_mut().unwrap();
    closed.related_ids.push(first.open_chapter.id.clone());
    closed
        .related_evidence
        .push("return to my earlier goal".into());
    store.save_episode_batch(&third)?;
    let closed_baseline =
        serde_json::to_value(store.context_subject_chapters(&session.id)?[0].clone())?;
    let mut fourth = batch(&session.id, 4, &history[6..8], 1, 1, Some(&history[5].id));
    assert!(store.save_episode_batch(&fourth).is_err());
    fourth.open_chapter = third.open_chapter.clone();
    fourth.open_chapter.last_episode_ordinal = 4;
    fourth.episode.chapter_id = fourth.open_chapter.id.clone();
    fourth.closed_chapter = Some(close(first.open_chapter));
    assert!(store.save_episode_batch(&fourth).is_err());
    assert_eq!(
        serde_json::to_value(store.context_subject_chapters(&session.id)?[0].clone())?,
        closed_baseline
    );
    Ok(())
}

#[tokio::test]
async fn v4_migration_preserves_legacy_json_and_starts_after_legacy_coverage() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("migration.sqlite");
    let store = Store::open(&path)?;
    let session = store.create_session("legacy")?;
    let history = canonical_history(&store, &session.id, 3).await?;
    let checkpoint = ContextCheckpoint {
        files_touched: Vec::new(),
        id: "legacy-checkpoint".into(),
        session_id: session.id.clone(),
        through_message_id: history[1].id.clone(),
        model: "v3-model".into(),
        summary: "frozen checkpoint".into(),
        created_at: history[1].created_at.clone(),
        usage: None,
    };
    store.save_context_checkpoint(&checkpoint)?;
    let legacy = ContextChapter {
        id: "legacy-chapter".into(),
        session_id: session.id.clone(),
        ordinal: 1,
        first_message_id: history[2].id.clone(),
        last_message_id: history[3].id.clone(),
        title: "Legacy chapter".into(),
        summary: "frozen chapter".into(),
        model: "v4-model".into(),
        usage: None,
        created_at: history[3].created_at.clone(),
    };
    store.save_context_chapter(&legacy)?;
    store.queue(&session.id, InputKind::Followup, "legacy pending input")?;
    drop(store);
    support::replace_with_legacy_fixture(&path, 4)?;
    let conn = rusqlite::Connection::open(&path)?;
    let read_legacy = |conn: &rusqlite::Connection| -> Result<Vec<serde_json::Value>> {
        let raw = [
            conn.query_row(
                "SELECT data FROM context_checkpoints WHERE session_id=?1",
                [&session.id],
                |r| r.get::<_, String>(0),
            )?,
            conn.query_row(
                "SELECT data FROM context_chapters WHERE session_id=?1",
                [&session.id],
                |r| r.get::<_, String>(0),
            )?,
        ];
        raw.into_iter()
            .map(|raw| {
                let mut value: serde_json::Value = serde_json::from_str(&raw)?;
                value.as_object_mut().unwrap().remove("usage");
                Ok(value)
            })
            .collect()
    };
    let bytes = read_legacy(&conn)?;
    drop(conn);
    let store = Store::open(&path)?;
    assert_eq!(store.storage_stats()?.chat_transaction_count, 0);
    assert_eq!(
        store.episode_coverage_token(&session.id)?,
        Some(history[3].id.clone())
    );
    assert_eq!(store.queued_inputs(&session.id)?[0].origin, None);
    assert_eq!(
        serde_json::to_value(store.context_checkpoint(&session.id)?)?,
        serde_json::to_value(Some(&checkpoint))?
    );
    assert_eq!(
        serde_json::to_value(store.context_chapters(&session.id)?)?,
        serde_json::to_value([&legacy])?
    );
    let wrong = batch(&session.id, 1, &history[2..6], 1, 1, Some(&history[3].id));
    assert!(store.save_episode_batch(&wrong).is_err());
    let next = batch(&session.id, 1, &history[4..6], 1, 1, Some(&history[3].id));
    store.save_episode_batch(&next)?;
    assert_eq!(store.storage_stats()?.chat_transaction_count, 1);
    let conn = rusqlite::Connection::open(store.chat_path(&session.id)?.unwrap())?;
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?,
        12
    );
    assert_eq!(read_legacy(&conn)?, bytes);
    // The legacy API remains available; automatic phase-3 compaction uses views.
    store.save_context_chapter(&ContextChapter {
        ordinal: 2,
        id: "overlap".into(),
        first_message_id: history[4].id.clone(),
        last_message_id: history[5].id.clone(),
        ..legacy
    })?;
    harness(&store)
        .resume(&session.id, RunOptions::default())
        .await?;
    let messages = store.messages(&session.id)?;
    let consumed = &messages[history.len()];
    assert_eq!(consumed.origin, None);
    assert!(is_typed_user(consumed));
    Ok(())
}

#[tokio::test]
async fn episode_coverage_can_start_after_checkpoint_without_v4_chapters() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("checkpoint")?;
    let history = canonical_history(&store, &session.id, 2).await?;
    let checkpoint = ContextCheckpoint {
        files_touched: Vec::new(),
        id: "cutoff".into(),
        session_id: session.id.clone(),
        through_message_id: history[1].id.clone(),
        model: "legacy".into(),
        summary: "legacy".into(),
        created_at: history[1].created_at.clone(),
        usage: None,
    };
    store.save_context_checkpoint(&checkpoint)?;
    let next = batch(&session.id, 1, &history[2..4], 1, 1, Some(&history[1].id));
    store.save_episode_batch(&next)?;
    assert!(
        store
            .save_context_checkpoint(&ContextCheckpoint {
                files_touched: Vec::new(),
                through_message_id: history[3].id.clone(),
                ..checkpoint
            })
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn empty_and_invalid_same_save_closes_roll_back_every_change() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("empty-close")?;
    let history = canonical_history(&store, &session.id, 1).await?;
    let first = batch(&session.id, 1, &history, 1, 1, None);
    let baseline = persisted_context(&store, &session.id)?;
    let before = store.storage_stats()?.chat_transaction_count;
    let mut empty = first.clone();
    empty.closed_chapter = Some(close(first.open_chapter.clone()));
    let error = store.save_episode_batch(&empty).unwrap_err();
    assert!(error.to_string().contains("no open chapter"));
    assert_eq!(persisted_context(&store, &session.id)?, baseline);
    assert_eq!(store.storage_stats()?.chat_transaction_count, before);

    let mut wrong = first.clone();
    let mut closing = close(first.open_chapter.clone());
    closing.id = "no-open-chapter".into();
    wrong.close_after_episode = Some(closing);
    assert!(store.save_episode_batch(&wrong).is_err());
    assert_eq!(persisted_context(&store, &session.id)?, baseline);
    assert_eq!(store.storage_stats()?.chat_transaction_count, before);

    let mut ambiguous = first;
    ambiguous.closed_chapter = empty.closed_chapter;
    ambiguous.close_after_episode = Some(close(ambiguous.open_chapter.clone()));
    assert!(store.save_episode_batch(&ambiguous).is_err());
    assert_eq!(persisted_context(&store, &session.id)?, baseline);
    assert_eq!(store.storage_stats()?.chat_transaction_count, before);
    Ok(())
}

#[tokio::test]
async fn reasoning_and_related_quote_result_round_trip_without_gating_on_match() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("metadata.sqlite");
    let store = Store::open(&path)?;
    let session = store.create_session("metadata")?;
    let history = canonical_history(&store, &session.id, 2).await?;
    let mut first = batch(&session.id, 1, &history[..2], 1, 1, None);
    first.episode.reasoning = Some("episode trace".into());
    first.decisions[0].reasoning = Some("chat decider trace".into());
    first.close_after_episode = Some(close(first.open_chapter.clone()));
    let before = store.storage_stats()?.chat_transaction_count;
    store.save_episode_batch(&first)?;
    let mut second = batch(&session.id, 2, &history[2..], 2, 2, Some(&history[1].id));
    let mut closing = close(second.open_chapter.clone());
    closing.reasoning = Some("chapter trace".into());
    closing.related_ids.push(first.open_chapter.id.clone());
    closing.related_evidence.push(RelatedEvidence {
        quote: "not found in user text".into(),
        related_quote_matched: Some(false),
        origin: Some(MessageOrigin::ChildTask),
    });
    second.close_after_episode = Some(closing);
    store.save_episode_batch(&second)?;
    assert_eq!(store.storage_stats()?.chat_transaction_count, before + 2);
    drop(store);
    let store = Store::open(&path)?;
    assert_eq!(
        store.context_episodes(&session.id)?[0].reasoning.as_deref(),
        Some("episode trace")
    );
    assert_eq!(
        store.context_decisions(&session.id)?[0]
            .reasoning
            .as_deref(),
        Some("chat decider trace")
    );
    let chapters = store.context_subject_chapters(&session.id)?;
    assert_eq!(chapters[1].reasoning.as_deref(), Some("chapter trace"));
    assert_eq!(
        chapters[1].related_evidence[0].origin,
        Some(MessageOrigin::ChildTask)
    );
    assert_eq!(
        chapters[1].related_evidence[0].related_quote_matched,
        Some(false)
    );
    assert_eq!(
        chapters[1].related_evidence[0].quote,
        "not found in user text"
    );
    assert!(store.open_subject_chapter(&session.id)?.is_none());
    Ok(())
}

#[tokio::test]
async fn phase_one_json_defaults_reasoning_and_accepts_legacy_string_evidence() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("legacy-json")?;
    let history = canonical_history(&store, &session.id, 1).await?;
    let first = batch(&session.id, 1, &history, 1, 1, None);
    let mut episode = serde_json::to_value(&first.episode)?;
    episode.as_object_mut().unwrap().remove("reasoning");
    assert!(
        serde_json::from_value::<ContextEpisode>(episode)?
            .reasoning
            .is_none()
    );
    let mut decision = serde_json::to_value(&first.decisions[0])?;
    decision.as_object_mut().unwrap().remove("reasoning");
    assert!(
        serde_json::from_value::<ContextDecision>(decision)?
            .reasoning
            .is_none()
    );
    let mut chapter = serde_json::to_value(close(first.open_chapter))?;
    chapter.as_object_mut().unwrap().remove("reasoning");
    chapter["related_ids"] = serde_json::json!(["previous"]);
    chapter["related_evidence"] = serde_json::json!(["the original user words"]);
    let chapter: ContextSubjectChapter = serde_json::from_value(chapter)?;
    assert!(chapter.reasoning.is_none());
    assert_eq!(chapter.related_evidence[0].quote, "the original user words");
    assert_eq!(chapter.related_evidence[0].related_quote_matched, None);
    assert_eq!(chapter.related_evidence[0].origin, None);
    let detailed: RelatedEvidence = serde_json::from_value(serde_json::json!({"quote":"words"}))?;
    assert_eq!(detailed.related_quote_matched, None);
    assert_eq!(detailed.origin, None);
    assert!(
        !serde_json::to_value(&detailed)?
            .as_object()
            .unwrap()
            .contains_key("origin")
    );
    Ok(())
}

#[tokio::test]
async fn child_task_anchor_and_assistant_episode_boundary_persist_without_forging_user_origin()
-> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("long child")?;
    let mut history = canonical_history(&store, &session.id, 2).await?;
    // Rebuild a canonical child assignment followed by work, without a typed request.
    let child = store.create_session("child-only source")?;
    history.truncate(3);
    for (index, message) in history.iter_mut().enumerate() {
        message.id = format!("child-source-{index}");
        message.session_id = child.id.clone();
        if index == 0 {
            message.origin = Some(MessageOrigin::ChildTask);
        } else {
            message.role = Role::Assistant;
            message.origin = None;
        }
    }
    store.append_batch(&history)?;
    let mut first = batch(&child.id, 1, &history[..2], 1, 1, None);
    first.decisions.clear();
    first.episode.origin_request_id = Some(history[0].id.clone());
    first.episode.cap = 1;
    first.episode.summary = "Long retained evidence. ".repeat(5000);
    store.save_episode_batch(&first)?;
    let mut next = batch(&child.id, 2, &history[2..], 1, 1, Some(&history[1].id));
    next.episode.origin_request_id = Some(history[0].id.clone());
    next.decisions[0].option_message_ids = vec![history[2].id.clone()];
    next.decisions[0].probabilities = vec![0.0, 1.0];
    let mut closed = close(next.open_chapter.clone());
    closed.summary = Some("Long chapter evidence. ".repeat(5000));
    next.close_after_episode = Some(closed);
    store.save_episode_batch(&next)?;
    assert_eq!(store.context_episodes(&child.id)?.len(), 2);
    assert_eq!(
        store.context_episodes(&child.id)?[0].summary,
        first.episode.summary
    );
    assert_eq!(
        store.context_subject_chapters(&child.id)?[0].summary,
        next.close_after_episode.unwrap().summary
    );
    let original = store.messages(&child.id)?;
    assert_eq!(original[0].origin, Some(MessageOrigin::ChildTask));
    assert!(!is_typed_user(&original[0]));
    assert_eq!(original[2].role, Role::Assistant);
    Ok(())
}
