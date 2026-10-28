use super::*;
use bubblepaw_core::{ContextEpisode, ContextEvent, ContextEventKind, ContextTrigger};
use bubblepaw_runner::episodes::{EpisodeConfig, EpisodeSummaryInput};

fn assert_safe_fact(event: &ContextEvent, session: &str) {
    assert_eq!(event.session_id, session);
    assert!(!event.id.is_empty());
    assert!(!event.created_at.is_empty());
    assert!(!event.trigger_text.trim().is_empty());
    assert!(event.trigger_text.chars().count() <= 160);
    for private_text in ["request 0", "datum", "Retained summary", "PRIVATE_FOCUS"] {
        assert!(!event.trigger_text.contains(private_text));
    }
    assert_eq!(event.input_before, None);
    assert_eq!(event.input_after, None);
    assert!(event.input_estimated);
}

fn text_tokens(text: &str) -> Result<u64> {
    let tokenizer = tiktoken_rs::bpe_for_tokenizer(tiktoken_rs::tokenizer::Tokenizer::O200kBase)?;
    Ok(tokenizer.encode_ordinary(text).len() as u64)
}

fn assert_episode_fact(event: &ContextEvent, episode: &ContextEpisode) -> Result<()> {
    assert_safe_fact(event, &episode.session_id);
    assert_eq!(event.kind, ContextEventKind::EpisodeWritten);
    assert_eq!(event.first_message_id, episode.first_message_id);
    assert_eq!(event.last_message_id, episode.last_message_id);
    assert_eq!(event.episode_ids, vec![episode.id.clone()]);
    assert_eq!(event.summary_id.as_deref(), Some(episode.id.as_str()));
    assert_eq!(event.tokens_before, Some(episode.source_tokens as u64));
    assert_eq!(event.tokens_after, text_tokens(&episode.summary)?);
    Ok(())
}

async fn short_fixture() -> Result<Fixture> {
    let fixture = Fixture::new(0).await?;
    let messages = [
        ("short-user", "user", "request 0 PRIVATE_SOURCE"),
        ("short-assistant", "assistant", "Completed the short task."),
    ]
    .into_iter()
    .map(|(id, role, content)| {
        serde_json::from_value(serde_json::json!({
            "id":id, "session_id":fixture.session, "role":role, "content":content,
            "created_at":"2026-10-27T00:00:00Z"
        }))
    })
    .collect::<serde_json::Result<Vec<Message>>>()?;
    fixture.store.append_batch(&messages)?;
    Ok(fixture)
}

#[tokio::test]
async fn ordinary_gathers_commit_source_size_facts_with_no_added_transactions() -> Result<()> {
    let fixture = Fixture::new(4).await?;
    let worker = fixture.worker(Arc::new(Models::default()));
    let before = fixture.store.storage_stats()?.transaction_count;
    worker.precompute(&fixture.session).await?;
    let episodes = fixture.store.context_episodes(&fixture.session)?;
    let events = fixture.store.context_events(&fixture.session)?;
    assert!(!episodes.is_empty());
    assert_eq!(events.len(), episodes.len());
    for (event, episode) in events.iter().zip(&episodes) {
        assert_episode_fact(event, episode)?;
        assert_eq!(event.trigger, ContextTrigger::SourceSize);
        assert_eq!(
            event.trigger_text,
            format!(
                "episode reached {} source tokens, target {}",
                episode.source_tokens,
                EpisodeConfig::default().source_target_tokens
            )
        );
    }
    let after = fixture.store.storage_stats()?.transaction_count;
    assert_eq!(after, before + episodes.len() as u64);
    worker.precompute(&fixture.session).await?;
    assert_eq!(fixture.store.context_events(&fixture.session)?, events);
    assert_eq!(fixture.store.storage_stats()?.transaction_count, after);
    Ok(())
}

#[tokio::test]
async fn decider_split_records_old_chapter_coverage_and_new_episode_subject_change() -> Result<()> {
    let fixture = Fixture::new(4).await?;
    let models = Arc::new(Models::default());
    models.split_once.store(true, Ordering::SeqCst);
    let worker = fixture.worker(models);
    let before = fixture.store.storage_stats()?.transaction_count;
    worker.precompute(&fixture.session).await?;
    let episodes = fixture.store.context_episodes(&fixture.session)?;
    let chapters = fixture.store.context_subject_chapters(&fixture.session)?;
    let events = fixture.store.context_events(&fixture.session)?;
    let closed = events
        .iter()
        .find(|event| event.kind == ContextEventKind::ChapterClosed)
        .unwrap();
    assert_safe_fact(closed, &fixture.session);
    assert_eq!(closed.trigger, ContextTrigger::SubjectChange);
    assert_eq!(closed.first_message_id, episodes[0].first_message_id);
    assert_eq!(closed.last_message_id, episodes[0].last_message_id);
    assert_eq!(closed.episode_ids, vec![episodes[0].id.clone()]);
    assert_eq!(closed.summary_id.as_deref(), Some(chapters[0].id.as_str()));
    assert_eq!(closed.tokens_before, Some(episodes[0].source_tokens as u64));
    assert_eq!(
        closed.tokens_after,
        text_tokens(chapters[0].summary.as_deref().unwrap())?
    );
    let new_episode = events
        .iter()
        .find(|event| event.summary_id.as_deref() == Some(episodes[1].id.as_str()))
        .unwrap();
    assert_episode_fact(new_episode, &episodes[1])?;
    assert_eq!(new_episode.trigger, ContextTrigger::SubjectChange);
    let seq = fixture
        .store
        .message_seq(&fixture.session, &episodes[1].first_message_id)?;
    assert_eq!(
        new_episode.trigger_text,
        format!("split before canonical message seq {seq}")
    );
    assert_eq!(closed.trigger_text, new_episode.trigger_text);
    assert_eq!(events.len(), episodes.len() + 1);
    assert_eq!(
        fixture.store.storage_stats()?.transaction_count,
        before + episodes.len() as u64
    );
    Ok(())
}

#[tokio::test]
async fn rollover_chapter_facts_cover_all_original_sources_including_new_episode() -> Result<()> {
    let fixture = Fixture::new(4).await?;
    let config = EpisodeConfig {
        summary_input: EpisodeSummaryInput::Episode,
        decider_enabled: false,
        chapter_rollover_tokens: text_tokens("# Episode 1\nRetained summary 1.")? as usize + 1,
        ..Default::default()
    };
    let worker = EpisodeWorker::new_with_config(
        fixture.store.clone(),
        Arc::new(Models::default()),
        fixture.estimator.clone(),
        config,
    )?;
    let before = fixture.store.storage_stats()?.transaction_count;
    worker.precompute(&fixture.session).await?;
    let episodes = fixture.store.context_episodes(&fixture.session)?;
    let chapters = fixture.store.context_subject_chapters(&fixture.session)?;
    let events = fixture.store.context_events(&fixture.session)?;
    let closed: Vec<_> = events
        .iter()
        .filter(|event| event.kind == ContextEventKind::ChapterClosed)
        .collect();
    assert!(!closed.is_empty());
    for event in closed {
        assert_safe_fact(event, &fixture.session);
        assert_eq!(event.trigger, ContextTrigger::SourceSize);
        let chapter = chapters
            .iter()
            .find(|chapter| Some(&chapter.id) == event.summary_id.as_ref())
            .unwrap();
        let members: Vec<_> = episodes
            .iter()
            .filter(|episode| episode.chapter_id == chapter.id)
            .collect();
        assert!(members.len() >= 2);
        assert_eq!(
            event.episode_ids,
            members
                .iter()
                .map(|episode| episode.id.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            event.first_message_id,
            members.first().unwrap().first_message_id
        );
        assert_eq!(
            event.last_message_id,
            members.last().unwrap().last_message_id
        );
        assert_eq!(
            event.tokens_before,
            Some(
                members
                    .iter()
                    .map(|episode| episode.source_tokens as u64)
                    .sum::<u64>()
            )
        );
        assert_eq!(
            event.tokens_after,
            text_tokens(chapter.summary.as_deref().unwrap())?
        );
        assert!(event.trigger_text.starts_with("chapter reached "));
    }
    assert_eq!(
        fixture.store.storage_stats()?.transaction_count,
        before + episodes.len() as u64
    );
    Ok(())
}

#[tokio::test]
async fn pressure_gather_records_pressure_and_stale_coverage_records_nothing() -> Result<()> {
    let fixture = Fixture::new(4).await?;
    let worker = fixture.worker(Arc::new(Models::default()));
    let end = fixture.store.messages(&fixture.session)?.len();
    let before = fixture.store.storage_stats()?.transaction_count;
    worker
        .gather_pressure_step(&fixture.session, end, false, None)
        .await?;
    let episodes = fixture.store.context_episodes(&fixture.session)?;
    let events = fixture.store.context_events(&fixture.session)?;
    assert_eq!(episodes.len(), 1);
    assert_eq!(events.len(), 1);
    assert_episode_fact(&events[0], &episodes[0])?;
    assert_eq!(events[0].trigger, ContextTrigger::Pressure);
    assert!(events[0].trigger_text.starts_with("pressure gather: "));
    assert_eq!(fixture.store.storage_stats()?.transaction_count, before + 1);
    worker
        .gather_pressure_step(&fixture.session, end, false, None)
        .await?;
    assert_eq!(fixture.store.context_events(&fixture.session)?, events);
    assert_eq!(fixture.store.storage_stats()?.transaction_count, before + 1);
    Ok(())
}

#[tokio::test]
async fn forced_short_gather_records_emergency_with_no_added_transaction() -> Result<()> {
    let fixture = short_fixture().await?;
    let worker = fixture.worker(Arc::new(Models::default()));
    let before = fixture.store.storage_stats()?.transaction_count;
    worker
        .gather_pressure_step(&fixture.session, 2, false, None)
        .await?;
    assert!(fixture.store.context_events(&fixture.session)?.is_empty());
    assert_eq!(fixture.store.storage_stats()?.transaction_count, before);
    worker
        .gather_pressure_step(&fixture.session, 2, true, None)
        .await?;
    let episodes = fixture.store.context_episodes(&fixture.session)?;
    let events = fixture.store.context_events(&fixture.session)?;
    assert_eq!(episodes.len(), 1);
    assert_eq!(events.len(), 1);
    assert_episode_fact(&events[0], &episodes[0])?;
    assert_eq!(events[0].trigger, ContextTrigger::Emergency);
    assert!(events[0].trigger_text.starts_with("emergency gather: "));
    assert_eq!(fixture.store.storage_stats()?.transaction_count, before + 1);
    Ok(())
}

#[tokio::test]
async fn manual_gather_fact_does_not_persist_focus_or_source_contents() -> Result<()> {
    let fixture = short_fixture().await?;
    let models = Arc::new(Models::default());
    let worker = fixture.worker(models.clone());
    let before = fixture.store.storage_stats()?.transaction_count;
    worker
        .compact_until(&fixture.session, 2, Some("PRIVATE_FOCUS"))
        .await?;
    let episodes = fixture.store.context_episodes(&fixture.session)?;
    let events = fixture.store.context_events(&fixture.session)?;
    assert_eq!(episodes.len(), 1);
    assert_eq!(events.len(), 1);
    assert_episode_fact(&events[0], &episodes[0])?;
    assert_eq!(events[0].trigger, ContextTrigger::Manual);
    assert_eq!(events[0].trigger_text, "user asked to compact");
    assert!(!events[0].trigger_text.contains("PRIVATE_SOURCE"));
    assert_eq!(
        models.summaries.lock().unwrap()[0].focus.as_deref(),
        Some("PRIVATE_FOCUS")
    );
    assert_eq!(fixture.store.storage_stats()?.transaction_count, before + 1);
    Ok(())
}

#[tokio::test]
async fn failed_episode_or_chapter_gather_commits_no_partial_facts() -> Result<()> {
    let fixture = Fixture::new(4).await?;
    let models = Arc::new(Models::default());
    models.blank_summary.store(true, Ordering::SeqCst);
    let worker = fixture.worker(models.clone());
    let before = fixture.store.storage_stats()?.transaction_count;
    assert!(worker.precompute(&fixture.session).await.is_err());
    assert!(fixture.store.context_events(&fixture.session)?.is_empty());
    assert_eq!(fixture.store.storage_stats()?.transaction_count, before);
    models.blank_summary.store(false, Ordering::SeqCst);
    models.split_once.store(true, Ordering::SeqCst);
    models.blank_chapter.store(true, Ordering::SeqCst);
    assert!(worker.precompute(&fixture.session).await.is_err());
    let episodes = fixture.store.context_episodes(&fixture.session)?;
    let events = fixture.store.context_events(&fixture.session)?;
    assert_eq!(episodes.len(), 1);
    assert_eq!(events.len(), 1);
    assert_episode_fact(&events[0], &episodes[0])?;
    assert_eq!(fixture.store.storage_stats()?.transaction_count, before + 1);
    Ok(())
}
