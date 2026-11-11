//! Chapter metadata compatibility keeps the existing test coverage unchanged.
use super::*;

#[tokio::test]
async fn invalid_summary_or_chapter_cannot_advance_canonical_coverage() -> Result<()> {
    let fixture = Fixture::new(4).await?;
    let original = fixture.store.messages(&fixture.session)?;
    let models = Arc::new(Models::default());
    models.blank_summary.store(true, Ordering::SeqCst);
    let worker = fixture.worker(models.clone());
    assert!(worker.precompute(&fixture.session).await.is_err());
    assert!(
        fixture
            .store
            .episode_coverage_token(&fixture.session)?
            .is_none()
    );
    assert!(fixture.store.context_episodes(&fixture.session)?.is_empty());
    assert!(fixture.store.context_view(&fixture.session)?.is_none());
    models.blank_summary.store(false, Ordering::SeqCst);
    models.protocol_summary.store(true, Ordering::SeqCst);
    assert!(worker.precompute(&fixture.session).await.is_err());
    assert!(
        fixture
            .store
            .episode_coverage_token(&fixture.session)?
            .is_none()
    );
    assert!(fixture.store.context_episodes(&fixture.session)?.is_empty());
    models.protocol_summary.store(false, Ordering::SeqCst);
    models.split_once.store(true, Ordering::SeqCst);
    models.blank_chapter.store(true, Ordering::SeqCst);
    assert!(worker.precompute(&fixture.session).await.is_err());
    let episodes = fixture.store.context_episodes(&fixture.session)?;
    assert_eq!(episodes.len(), 1);
    assert_eq!(
        fixture.store.episode_coverage_token(&fixture.session)?,
        Some(episodes[0].last_message_id.clone())
    );
    assert_eq!(
        fixture.store.context_subject_chapters(&fixture.session)?[0].status,
        SubjectChapterStatus::Open
    );
    assert!(
        fixture
            .store
            .context_decisions(&fixture.session)?
            .is_empty()
    );
    assert_eq!(fixture.store.messages(&fixture.session)?, original);
    models.blank_chapter.store(false, Ordering::SeqCst);
    worker.precompute(&fixture.session).await?;
    assert!(fixture.store.context_episodes(&fixture.session)?.len() > 1);
    assert!(fixture.store.context_view(&fixture.session)?.is_none());
    Ok(())
}
