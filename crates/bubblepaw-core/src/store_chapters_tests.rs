use super::*;

fn chapter() -> ContextSubjectChapter {
    ContextSubjectChapter {
        id: "chapter".into(),
        session_id: "session".into(),
        ordinal: 1,
        first_episode_ordinal: 7,
        last_episode_ordinal: 10,
        status: SubjectChapterStatus::Closed,
        description: Some("Work".into()),
        summary: Some("Memory".into()),
        decisions: None,
        sections: None,
        related_ids: vec![],
        related_evidence: vec![],
        model: Some("scripted".into()),
        usage: None,
        reasoning: None,
    }
}

#[test]
fn old_json_round_trips_without_optional_keys_and_valid_none_stays_explicit() -> Result<()> {
    let original = serde_json::to_value(chapter())?;
    assert!(original.get("decisions").is_none());
    assert!(original.get("sections").is_none());
    let mut restored: ContextSubjectChapter = serde_json::from_value(original.clone())?;
    assert_eq!(serde_json::to_value(&restored)?, original);
    restored.decisions = Some(vec![]);
    restored.sections = Some(vec![ChapterSection {
        name: "Implementation".into(),
        first_episode: 7,
        last_episode: 10,
    }]);
    restored.validate_retrieval_metadata()?;
    let structured = serde_json::to_value(&restored)?;
    assert_eq!(structured["decisions"], serde_json::json!([]));
    assert_eq!(structured["sections"][0]["first_episode"], 7);
    assert_eq!(structured["sections"][0]["last_episode"], 10);
    Ok(())
}

#[test]
fn metadata_requires_a_closed_chapter_and_exact_tiling() -> Result<()> {
    let mut subject = chapter();
    subject.decisions = Some(vec!["Keep the fix.".into()]);
    assert!(subject.validate_retrieval_metadata().is_err());
    subject.sections = Some(vec![ChapterSection {
        name: "Fix".into(),
        first_episode: 7,
        last_episode: 10,
    }]);
    subject.validate_retrieval_metadata()?;
    subject.status = SubjectChapterStatus::Open;
    assert!(subject.validate_retrieval_metadata().is_err());
    subject.status = SubjectChapterStatus::Closed;
    subject.sections.as_mut().unwrap()[0].last_episode = 9;
    assert!(subject.validate_retrieval_metadata().is_err());
    subject.sections.as_mut().unwrap()[0].last_episode = 10;
    subject.decisions = Some(vec!["a".repeat(101)]);
    assert!(subject.validate_retrieval_metadata().is_err());
    subject.decisions = Some(vec!["first\nsecond".into()]);
    assert!(subject.validate_retrieval_metadata().is_err());
    Ok(())
}
