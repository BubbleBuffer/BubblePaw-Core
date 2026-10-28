//! Episode planning and closed-chapter synthesis.
use super::*;
use crate::episodes::PlannedEpisode;

pub(super) fn next_episode(
    snapshot: &HistorySnapshot,
    coverage_end: usize,
    end_limit: Option<usize>,
    manual: bool,
    config: &EpisodeConfig,
) -> Result<Option<PlannedEpisode>> {
    let ordinary = snapshot
        .plan_with_config(coverage_end, config)?
        .episodes
        .into_iter()
        .next();
    Ok(match ordinary {
        Some(planned) if end_limit.is_none_or(|limit| planned.message_range.end <= limit) => {
            Some(planned)
        }
        _ if manual => snapshot.manual_episode(coverage_end, end_limit.unwrap()),
        _ => None,
    })
}

impl EpisodeWorker {
    pub(super) async fn close_chapter(
        &self,
        session: &str,
        mut chapter: ContextSubjectChapter,
        episodes: &[ContextEpisode],
        chapters: &[ContextSubjectChapter],
        snapshot: &HistorySnapshot,
    ) -> Result<ContextSubjectChapter> {
        let members: Vec<_> = episodes
            .iter()
            .filter(|e| e.chapter_id == chapter.id)
            .collect();
        let first = members
            .first()
            .context("Cannot close an empty subject chapter")?;
        let last = members.last().unwrap();
        let start = snapshot.position(&first.first_message_id)?;
        let end = snapshot.position(&last.last_message_id)?;
        let mut typed_user_texts = Vec::new();
        let mut task_instructions = Vec::new();
        let mut collect_instruction = |message: Message| {
            if is_typed_user(&message) {
                typed_user_texts.push(message.content);
            } else if message.role == Role::User && message.origin == Some(MessageOrigin::ChildTask)
            {
                task_instructions.push(message.content);
            }
        };
        // A continuation chapter may start after its task anchor was covered.
        // Read the actual instruction once; never reclassify it as human input.
        if let Some(origin) = &first.origin_request_id {
            let index = snapshot.position(origin)?;
            if index < start {
                collect_instruction(snapshot.message(index)?);
            }
        }
        snapshot.walk(start, end + 1, |_, message| {
            collect_instruction(message);
            Ok(())
        })?;
        let previous_chapters = chapters
            .iter()
            .filter(|c| c.status == SubjectChapterStatus::Closed)
            .rev()
            .take(8)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let result = self
            .chapter_model(
                session,
                &last.last_message_id,
                ChapterInput {
                    session_id: session.into(),
                    episodes: members
                        .iter()
                        .map(|e| {
                            format!(
                                "EPISODE {} [source {}..{}]\n{}",
                                e.ordinal, e.first_message_id, e.last_message_id, e.summary
                            )
                        })
                        .collect(),
                    previous_chapters,
                    typed_user_texts,
                    task_instructions,
                },
            )
            .await?;
        anyhow::ensure!(
            !result.description.trim().is_empty() && !result.summary.trim().is_empty(),
            "Chapter summarization returned an empty description or summary"
        );
        if let Some(reason) =
            crate::episode_models::summary_output_reason("chapter", &result.summary)
        {
            anyhow::bail!("Chapter summarization returned invalid memory: {reason}");
        }
        chapter.status = SubjectChapterStatus::Closed;
        chapter.description = Some(result.description);
        // The code owns the retrieval label; avoid duplicating a model-echoed label.
        let label = format!("[ch-{}]", chapter.ordinal);
        let mut summary = result.summary.trim_start();
        while let Some(rest) = summary.strip_prefix(&label) {
            if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
                break;
            }
            summary = rest.trim_start();
        }
        anyhow::ensure!(
            !summary.trim().is_empty(),
            "Chapter summarization returned no memory after its retrieval label"
        );
        chapter.summary = Some(format!("{label}\n{summary}"));
        chapter.decisions = result.decisions;
        chapter.sections = result.sections;
        chapter.related_ids = result.related_ids;
        chapter.related_evidence = result.related_evidence;
        chapter.model = Some(result.model);
        chapter.usage = Some(result.usage);
        chapter.reasoning = result.reasoning;
        Ok(chapter)
    }
}

pub(super) fn new_open_chapter(
    session: &str,
    chapter_ordinal: usize,
    episode_ordinal: usize,
) -> ContextSubjectChapter {
    ContextSubjectChapter {
        id: new_id(),
        session_id: session.into(),
        ordinal: chapter_ordinal,
        first_episode_ordinal: episode_ordinal,
        last_episode_ordinal: episode_ordinal,
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
    }
}
