//! Chapter metadata compatibility keeps the existing test coverage unchanged.
use super::*;

#[tokio::test]
async fn invalid_stored_episode_or_chapter_is_rejected_at_apply_without_changing_existing_view()
-> Result<()> {
    for closed in [false, true] {
        let f = Fixture::new()?;
        let history = f.turns(4, 35_000);
        f.seed(&history)?;
        f.save_episodes(&history, 1, 0, false)?;
        let policy = f.policy(Script::normal(), 110_000)?;
        let first = f.prepare(&policy).await?;
        let previous_view = serde_json::to_vec(&f.store.context_view(&f.session)?)?;
        let mut chapter = f.store.context_subject_chapters(&f.session)?[0].clone();
        chapter.last_episode_ordinal = 2;
        let mut episode = f.store.context_episodes(&f.session)?[0].clone();
        episode.id = id();
        episode.ordinal = 2;
        episode.first_message_id = history[2].id.clone();
        episode.last_message_id = history[3].id.clone();
        let protocol = r#"<tool_call><invoke name="read"/></tool_call>"#;
        episode.summary = if closed {
            "Valid second episode memory".into()
        } else {
            protocol.into()
        };
        let closed_chapter = closed.then(|| {
            let mut completed = chapter.clone();
            completed.status = SubjectChapterStatus::Closed;
            completed.description = Some("Legacy invalid body".into());
            completed.summary = Some(protocol.into());
            completed.model = Some("old-model".into());
            completed
        });
        // Legacy records can pass storage's structural contract. They must still
        // pass the same semantic-output validation before a new apply serves them.
        f.store.save_episode_batch(&EpisodeBatch {
            episode,
            decisions: vec![],
            open_chapter: chapter,
            closed_chapter: None,
            close_after_episode: closed_chapter,
            expected_previous_coverage: f.store.episode_coverage_token(&f.session)?,
        })?;
        let below_pressure = f.prepare(&policy).await?;
        assert!(below_pressure.notice.is_none());
        assert_eq!(below_pressure.request.messages, first.request.messages);
        f.seed(&f.turns(1, 35_000))?;
        let original_history = f.store.messages(&f.session)?;
        let original_coverage = f.store.episode_coverage_token(&f.session)?;
        let error = match f.prepare(&policy).await {
            Err(error) => error,
            Ok(_) => panic!("stored protocol output is invalid memory"),
        };
        assert!(
            error
                .to_string()
                .contains("requires repair (tool_protocol_output)")
        );
        assert_eq!(
            serde_json::to_vec(&f.store.context_view(&f.session)?)?,
            previous_view
        );
        assert_eq!(
            f.store.episode_coverage_token(&f.session)?,
            original_coverage
        );
        assert_eq!(f.store.messages(&f.session)?, original_history);
    }
    Ok(())
}
