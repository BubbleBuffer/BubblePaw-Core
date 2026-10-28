//! Gather facts are committed with their episode, never as a separate write.
use super::*;

pub(super) fn gather_events(
    batch: &EpisodeBatch,
    previous: &[ContextEpisode],
    trigger: ContextTrigger,
    split_before_seq: Option<i64>,
    config: &EpisodeConfig,
) -> Result<Vec<ContextEvent>> {
    let episode = &batch.episode;
    let (episode_trigger, trigger_text) = match split_before_seq {
        Some(seq) => (
            ContextTrigger::SubjectChange,
            format!("split before canonical message seq {seq}"),
        ),
        None => (
            trigger,
            match trigger {
                ContextTrigger::Manual => "user asked to compact".into(),
                ContextTrigger::Pressure => format!(
                    "pressure gather: {} source tokens, target {}, maximum {}",
                    episode.source_tokens, config.source_target_tokens, config.source_max_tokens
                ),
                ContextTrigger::Emergency => format!(
                    "emergency gather: {} source tokens, target {}, maximum {}",
                    episode.source_tokens, config.source_target_tokens, config.source_max_tokens
                ),
                ContextTrigger::SourceSize | ContextTrigger::SubjectChange => format!(
                    "episode reached {} source tokens, target {}",
                    episode.source_tokens, config.source_target_tokens
                ),
            },
        ),
    };
    let mut events = Vec::with_capacity(2);
    let chapter_event = |chapter: &ContextSubjectChapter,
                         trigger: ContextTrigger,
                         trigger_text: String|
     -> Result<ContextEvent> {
        // A phase decision closes the old chapter, excluding the newly written
        // episode; volume rollover closes the current chapter including it.
        let members: Vec<_> = previous
            .iter()
            .chain(std::iter::once(episode))
            .filter(|member| member.chapter_id == chapter.id)
            .collect();
        let first = members.first().context("Chapter fact has no episodes")?;
        let last = members.last().unwrap();
        let summary = chapter
            .summary
            .as_deref()
            .context("Chapter fact has no summary")?;
        Ok(ContextEvent {
            id: new_id(),
            session_id: episode.session_id.clone(),
            kind: ContextEventKind::ChapterClosed,
            created_at: now(),
            trigger,
            trigger_text,
            first_message_id: first.first_message_id.clone(),
            last_message_id: last.last_message_id.clone(),
            episode_ids: members.iter().map(|member| member.id.clone()).collect(),
            summary_id: Some(chapter.id.clone()),
            tokens_before: Some(members.iter().fold(0_u64, |total, member| {
                total.saturating_add(member.source_tokens as u64)
            })),
            tokens_after: text_tokens(summary) as u64,
            input_before: None,
            input_after: None,
            input_estimated: true,
        })
    };
    if let Some(chapter) = &batch.closed_chapter {
        events.push(chapter_event(
            chapter,
            ContextTrigger::SubjectChange,
            trigger_text.clone(),
        )?);
    }
    // The new episode starts the new subject when a decider split was selected.
    // Its range remains its own source range, never the old chapter's range.
    events.push(ContextEvent {
        id: new_id(),
        session_id: episode.session_id.clone(),
        kind: ContextEventKind::EpisodeWritten,
        created_at: episode.created_at.clone(),
        trigger: episode_trigger,
        trigger_text,
        first_message_id: episode.first_message_id.clone(),
        last_message_id: episode.last_message_id.clone(),
        episode_ids: vec![episode.id.clone()],
        summary_id: Some(episode.id.clone()),
        tokens_before: Some(episode.source_tokens as u64),
        tokens_after: text_tokens(&episode.summary) as u64,
        input_before: None,
        input_after: None,
        input_estimated: true,
    });
    if let Some(chapter) = &batch.close_after_episode {
        let summary_tokens = previous
            .iter()
            .chain(std::iter::once(episode))
            .filter(|member| member.chapter_id == chapter.id)
            .fold(0_usize, |total, member| {
                total.saturating_add(text_tokens(&member.summary))
            });
        events.push(chapter_event(
            chapter,
            ContextTrigger::SourceSize,
            format!(
                "chapter reached {summary_tokens} summary tokens, rollover {}",
                config.chapter_rollover_tokens
            ),
        )?);
    }
    Ok(events)
}
