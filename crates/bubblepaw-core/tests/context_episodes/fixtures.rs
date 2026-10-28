use super::*;

pub(super) async fn canonical_history(
    store: &Store,
    session: &str,
    turns: usize,
) -> Result<Vec<Message>> {
    let foreign = store.create_session_with_parent("foreign", Default::default(), Some(session))?;
    let agent = harness(store);
    for index in 0..turns {
        agent
            .prompt(session, format!("request {index}"), RunOptions::default())
            .await?;
        agent
            .prompt(&foreign.id, "foreign request", RunOptions::default())
            .await?;
    }
    store.messages(session)
}

pub(super) fn batch(
    session: &str,
    ordinal: usize,
    range: &[Message],
    chapter_ordinal: usize,
    chapter_first: usize,
    expected: Option<&str>,
) -> EpisodeBatch {
    let chapter_id = format!("{session}:subject:{chapter_ordinal}");
    let options: Vec<_> = range
        .iter()
        .filter(|message| is_typed_user(message))
        .map(|message| message.id.clone())
        .collect();
    let mut probabilities = vec![0.0; options.len()];
    probabilities.push(1.0);
    EpisodeBatch {
        episode: ContextEpisode {
            id: format!("{session}:episode:{ordinal}"),
            session_id: session.into(),
            ordinal,
            first_message_id: range.first().unwrap().id.clone(),
            last_message_id: range.last().unwrap().id.clone(),
            source_tokens: 32_000,
            cap: 2_000,
            origin_request_id: None,
            title: format!("Episode {ordinal}"),
            summary: format!("Facts retained from episode {ordinal}."),
            model: "summary-model".into(),
            usage: Some(Usage {
                input_tokens: 32_000,
                output_tokens: 2_100,
                ..Default::default()
            }),
            reasoning: None,
            created_at: range.last().unwrap().created_at.clone(),
            chapter_id: chapter_id.clone(),
        },
        decisions: vec![ContextDecision {
            id: format!("{session}:decision:{ordinal}"),
            session_id: session.into(),
            kind: DecisionKind::Split,
            episode_ordinal: ordinal,
            option_message_ids: options,
            probabilities,
            choice: None,
            model: "decider-model".into(),
            usage: None,
            reasoning: None,
            created_at: range.last().unwrap().created_at.clone(),
        }],
        open_chapter: ContextSubjectChapter {
            id: chapter_id,
            session_id: session.into(),
            ordinal: chapter_ordinal,
            first_episode_ordinal: chapter_first,
            last_episode_ordinal: ordinal,
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
        closed_chapter: None,
        close_after_episode: None,
        expected_previous_coverage: expected.map(str::to_owned),
    }
}

pub(super) fn close(mut chapter: ContextSubjectChapter) -> ContextSubjectChapter {
    chapter.status = SubjectChapterStatus::Closed;
    chapter.description = Some("Subject completed".into());
    chapter.summary = Some("The subject's decisions and results.".into());
    chapter.model = Some("chapter-model".into());
    chapter.usage = Some(Usage {
        input_tokens: 4_000,
        output_tokens: 100,
        ..Default::default()
    });
    chapter
}

pub(super) fn persisted_context(store: &Store, session: &str) -> Result<serde_json::Value> {
    Ok(serde_json::json!({
        "episodes": store.context_episodes(session)?,
        "chapters": store.context_subject_chapters(session)?,
        "decisions": store.context_decisions(session)?,
    }))
}

pub(super) fn view(session: &str, frontier: Option<i64>, prefix: Message) -> ContextView {
    ContextView {
        session_id: session.into(),
        frontier,
        system_prompt: "Frozen fixture instructions".into(),
        prefix: vec![prefix],
        masked_tool_output_ids: Vec::new(),
        clipped_tool_call_ids: Vec::new(),
        created_at: "2026-10-08T00:00:00Z".into(),
        threshold: 200_000,
        budget: 100_000,
        tokens: 40_000,
        blocks: Vec::new(),
        blocks_estimated: true,
    }
}
