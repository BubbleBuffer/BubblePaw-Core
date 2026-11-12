use super::*;

#[tokio::test]
async fn chapter_ids_are_frozen_with_summary_text_across_later_turns() -> Result<()> {
    let fixture = Fixture::new()?;
    let history = fixture.turns(8, 35000);
    fixture.seed(&history)?;
    fixture.save_episodes(&history, 7, 6000, true)?;
    let policy = fixture.policy(Script::normal(), 256000)?;
    let first = fixture.prepare(&policy).await?;
    let view = fixture.store.context_view(&fixture.session)?.unwrap();
    let summary = view
        .prefix
        .iter()
        .find(|message| message.content.starts_with("Closed chapter summaries"))
        .unwrap();
    assert!(summary.content.contains("[ch-1]\nFUTURE_CHAPTER_SECRET"));
    assert_eq!(summary.content.matches("[ch-1]").count(), 1);
    assert!(
        view.prefix
            .iter()
            .any(|message| message.content.contains("[ch-1] Chapter 1"))
    );
    let frozen = serde_json::to_vec(&view.prefix)?;
    let declarations = serde_json::to_vec(&first.request.tools)?;
    fixture.seed(&[
        fixture.message(Role::User, "Continue with a new task."),
        fixture.message(Role::Assistant, "The next task is complete."),
    ])?;
    let later = fixture.prepare(&policy).await?;
    let later_view = fixture.store.context_view(&fixture.session)?.unwrap();
    assert_eq!(frozen, serde_json::to_vec(&later_view.prefix)?);
    assert_eq!(declarations, serde_json::to_vec(&later.request.tools)?);
    assert!(
        later
            .request
            .messages
            .iter()
            .any(|message| message == summary)
    );
    Ok(())
}

/// A 256k-threshold policy with the given closed-chapter budget; 0 serves them all.
pub(super) fn policy_with_chapter_budget(
    fixture: &Fixture,
    percent: usize,
) -> Result<SummarizingContext> {
    SummarizingContext::new(
        fixture.store.clone(),
        Script::normal(),
        SummaryConfig {
            threshold_cap: 256000,
            lcm: bubblepaw_runner::summarization::LcmConfig {
                output_reserve_tokens: 0,
                episodes: bubblepaw_runner::episodes::EpisodeConfig {
                    chapter_budget_percent: percent,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        },
        None,
    )
}

/// Four closed one-episode chapters of about 4000 summary tokens each, over a
/// 256k threshold: `chapter_budget_percent` decides which summaries the applied
/// view keeps. Every chapter keeps its index row either way.
#[tokio::test]
async fn closed_chapter_summaries_are_capped_oldest_first_and_keep_their_index_rows() -> Result<()>
{
    // (percent, chapters whose summary stays). 6% of 256k is ~15.4k tokens: three
    // chapters fit, four do not; 1% fits none, so only the newest stays.
    let cases: [(usize, &[usize]); 4] = [
        (10, &[1, 2, 3, 4]),
        (6, &[2, 3, 4]),
        (1, &[4]),
        (0, &[1, 2, 3, 4]),
    ];
    for (percent, kept) in cases {
        let fixture = Fixture::new()?;
        let history = fixture.turns(8, 35000);
        fixture.seed(&history)?;
        for n in 1..=4usize {
            let chapter = ContextSubjectChapter {
                id: id(),
                session_id: fixture.session.clone(),
                ordinal: n,
                first_episode_ordinal: n,
                last_episode_ordinal: n,
                status: SubjectChapterStatus::Closed,
                description: Some("subject".into()),
                summary: Some(format!("CHSUM{n}{}", " datum".repeat(4000))),
                related_ids: vec![],
                related_evidence: vec![],
                decisions: None,
                sections: None,
                model: Some("fake".into()),
                usage: None,
                reasoning: None,
            };
            fixture.store.save_episode_batch(&EpisodeBatch {
                episode: ContextEpisode {
                    id: id(),
                    session_id: fixture.session.clone(),
                    ordinal: n,
                    first_message_id: history[2 * (n - 1)].id.clone(),
                    last_message_id: history[2 * n - 1].id.clone(),
                    source_tokens: 35000,
                    cap: 2200,
                    origin_request_id: None,
                    title: format!("title {n}"),
                    summary: format!("episode {n}"),
                    model: "fake".into(),
                    usage: None,
                    reasoning: None,
                    created_at: now(),
                    chapter_id: chapter.id.clone(),
                },
                decisions: vec![],
                open_chapter: ContextSubjectChapter {
                    status: SubjectChapterStatus::Open,
                    description: None,
                    summary: None,
                    model: None,
                    ..chapter.clone()
                },
                closed_chapter: None,
                close_after_episode: Some(chapter),
                expected_previous_coverage: fixture
                    .store
                    .episode_coverage_token(&fixture.session)?,
            })?;
        }
        let policy = policy_with_chapter_budget(&fixture, percent)?;
        fixture.prepare(&policy).await?;
        let view = fixture.store.context_view(&fixture.session)?.unwrap();
        let text = |start: &str| {
            view.prefix
                .iter()
                .find(|message| message.content.starts_with(start))
                .map(|message| message.content.clone())
                .unwrap_or_default()
        };
        let summaries = text("Closed chapter summaries");
        let index = text("Subject chapter index");
        for n in 1..=4usize {
            assert_eq!(
                summaries.contains(&format!("CHSUM{n}")),
                kept.contains(&n),
                "percent {percent}, chapter {n}"
            );
            let row = index
                .lines()
                .find(|line| line.starts_with(&format!("[ch-{n}] ")))
                .unwrap_or_else(|| panic!("percent {percent}: no index row for chapter {n}"));
            assert_eq!(
                row.contains("summary left out"),
                !kept.contains(&n),
                "percent {percent}, chapter {n}"
            );
        }
    }
    Ok(())
}
