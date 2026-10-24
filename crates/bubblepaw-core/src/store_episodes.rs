//! Atomic episode and subject chapter persistence.
use super::*;

impl Store {
    /// Append exactly one immutable episode, its decisions, and one chapter transition.
    /// Every validation and write shares a transaction, including the coverage check.
    pub fn save_episode_batch(&self, batch: &EpisodeBatch) -> Result<()> {
        if let Some(store) = self.scoped(&batch.episode.session_id)? {
            return store.save_episode_batch(batch);
        }
        let episode = &batch.episode;
        anyhow::ensure!(
            batch.closed_chapter.is_none() || batch.close_after_episode.is_none(),
            "Batch cannot close both a previous chapter and the inserted episode chapter"
        );
        anyhow::ensure!(!episode.id.trim().is_empty(), "Episode ID is empty");
        anyhow::ensure!(
            !episode.session_id.trim().is_empty(),
            "Episode session ID is empty"
        );
        anyhow::ensure!(
            !episode.chapter_id.trim().is_empty(),
            "Episode chapter ID is empty"
        );
        anyhow::ensure!(
            !episode.title.trim().is_empty() && !episode.title.contains(['\r', '\n']),
            "Episode title must be nonempty and single-line"
        );
        anyhow::ensure!(
            !episode.summary.trim().is_empty(),
            "Episode summary must be nonempty"
        );
        anyhow::ensure!(!episode.model.trim().is_empty(), "Episode model is empty");
        anyhow::ensure!(
            !episode.created_at.trim().is_empty(),
            "Episode creation time is empty"
        );
        let ordinal = i64::try_from(episode.ordinal).context("Episode ordinal is too large")?;
        let data = serde_json::to_string(episode)?;
        self.write_context(|tx| {
            let (previous_ordinal, previous_last, token) = Self::episode_coverage(tx, &episode.session_id)?;
            anyhow::ensure!(token == batch.expected_previous_coverage, "Stale episode coverage");
            anyhow::ensure!(previous_ordinal.checked_add(1) == Some(ordinal), "Episode ordinal must follow previous coverage");
            let first = Self::session_message_seq(tx, &episode.session_id, &episode.first_message_id)?;
            let last = Self::session_message_seq(tx, &episode.session_id, &episode.last_message_id)?;
            anyhow::ensure!(first <= last, "Episode range cannot regress");
            let next: Option<i64> = tx.query_row(
                "SELECT MIN(seq) FROM messages WHERE session_id=?1 AND seq>?2",
                params![episode.session_id, previous_last],
                |row| row.get(0),
            )?;
            anyhow::ensure!(next == Some(first), "Episode range must start at the next session message after existing coverage");
            if let Some(request_id) = &episode.origin_request_id {
                let seq = Self::session_message_seq(tx, &episode.session_id, request_id)?;
                let message: String = tx.query_row("SELECT data FROM messages WHERE seq=?1", [seq], |row| row.get(0))?;
                let anchor = Self::message_from_data(tx, &message)?;
                let is_task = is_typed_user(&anchor)
                    || (anchor.role == Role::User && anchor.origin == Some(crate::MessageOrigin::ChildTask));
                anyhow::ensure!(seq <= first && is_task, "Episode origin request must be a preceding typed request or child task in the same session");
            }

            Self::save_episode_chapter_change(tx, batch)?;
            tx.execute(
                "INSERT INTO context_episodes(id,session_id,ordinal,first_message_seq,last_message_seq,chapter_id,data) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![episode.id, episode.session_id, ordinal, first, last, episode.chapter_id, data],
            )?;
            if let Some(closed) = &batch.close_after_episode {
                Self::close_episode_chapter(tx, &batch.open_chapter, closed)?;
            }
            for decision in &batch.decisions {
                Self::validate_episode_decision(tx, decision, episode, first)?;
                tx.execute(
                    "INSERT INTO context_decisions(id,session_id,episode_ordinal,kind,data) VALUES(?1,?2,?3,'split',?4)",
                    params![decision.id, decision.session_id, ordinal, serde_json::to_string(decision)?],
                )?;
            }
            Ok(())
        })
    }

    pub(super) fn session_message_seq(
        conn: &Connection,
        session_id: &str,
        message_id: &str,
    ) -> Result<i64> {
        conn.query_row(
            "SELECT seq FROM messages WHERE session_id=?1 AND message_id=?2",
            params![session_id, message_id],
            |row| row.get(0),
        )
        .optional()?
        .context("Message does not belong to the session")
    }

    fn save_episode_chapter_change(tx: &Connection, batch: &EpisodeBatch) -> Result<()> {
        let chapter = &batch.open_chapter;
        let episode = &batch.episode;
        Self::validate_subject_chapter(chapter)?;
        anyhow::ensure!(
            chapter.status == SubjectChapterStatus::Open,
            "Batch requires an open chapter"
        );
        anyhow::ensure!(
            chapter.session_id == episode.session_id && chapter.id == episode.chapter_id,
            "Episode membership must reference this session's open chapter"
        );
        anyhow::ensure!(
            chapter.last_episode_ordinal == episode.ordinal,
            "Chapter must end at the inserted episode"
        );
        let previous = Self::read_open_subject_chapter(tx, &episode.session_id)?;
        match (previous, &batch.closed_chapter) {
            (Some(previous), None) => {
                anyhow::ensure!(
                    chapter.id == previous.id
                        && chapter.ordinal == previous.ordinal
                        && chapter.first_episode_ordinal == previous.first_episode_ordinal
                        && previous.last_episode_ordinal.checked_add(1) == Some(episode.ordinal),
                    "Only the open chapter may extend, with consecutive membership"
                );
                tx.execute(
                    "UPDATE context_subject_chapters SET last_episode_ordinal=?2,data=?3 WHERE id=?1 AND status='open'",
                    params![chapter.id, i64::try_from(chapter.last_episode_ordinal)?, serde_json::to_string(chapter)?],
                )?;
            }
            (Some(previous), Some(closed)) => {
                Self::validate_subject_chapter(closed)?;
                anyhow::ensure!(
                    closed.status == SubjectChapterStatus::Closed
                        && closed.id == previous.id
                        && closed.session_id == previous.session_id
                        && closed.ordinal == previous.ordinal
                        && closed.first_episode_ordinal == previous.first_episode_ordinal
                        && closed.last_episode_ordinal == previous.last_episode_ordinal,
                    "Only the open chapter can close, without changing membership"
                );
                anyhow::ensure!(
                    previous.ordinal.checked_add(1) == Some(chapter.ordinal)
                        && previous.last_episode_ordinal.checked_add(1) == Some(episode.ordinal)
                        && chapter.first_episode_ordinal == episode.ordinal
                        && chapter.id != previous.id,
                    "New chapter membership must follow the closed chapter"
                );
                Self::close_episode_chapter(tx, &previous, closed)?;
                Self::insert_subject_chapter(tx, chapter)?;
            }
            (None, None) => {
                let (previous_ordinal, previous_last): (i64, i64) = tx.query_row(
                    "SELECT COALESCE(MAX(ordinal),0),COALESCE(MAX(last_episode_ordinal),0) FROM context_subject_chapters WHERE session_id=?1",
                    [&episode.session_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                anyhow::ensure!(
                    i64::try_from(chapter.ordinal)? == previous_ordinal + 1
                        && i64::try_from(episode.ordinal)? == previous_last + 1
                        && chapter.first_episode_ordinal == episode.ordinal,
                    "New chapter must follow existing coverage and start at the inserted episode"
                );
                Self::insert_subject_chapter(tx, chapter)?;
            }
            (None, Some(_)) => bail!("There is no open chapter to close"),
        }
        Ok(())
    }

    fn close_episode_chapter(
        tx: &Connection,
        open: &ContextSubjectChapter,
        closed: &ContextSubjectChapter,
    ) -> Result<()> {
        Self::validate_subject_chapter(closed)?;
        anyhow::ensure!(
            closed.status == SubjectChapterStatus::Closed
                && closed.id == open.id
                && closed.session_id == open.session_id
                && closed.ordinal == open.ordinal
                && closed.first_episode_ordinal == open.first_episode_ordinal
                && closed.last_episode_ordinal == open.last_episode_ordinal,
            "Only the open chapter can close, without changing membership"
        );
        for related_id in &closed.related_ids {
            let related: Option<i64> = tx.query_row(
                "SELECT ordinal FROM context_subject_chapters WHERE id=?1 AND session_id=?2 AND status='closed'",
                params![related_id, closed.session_id], |row| row.get(0),
            ).optional()?;
            anyhow::ensure!(
                related.is_some_and(|ordinal| ordinal < closed.ordinal as i64),
                "Related chapters must be backward, closed and in the same session"
            );
        }
        let changed = tx.execute(
            "UPDATE context_subject_chapters SET status='closed',data=?2 WHERE id=?1 AND session_id=?3 AND status='open'",
            params![closed.id, serde_json::to_string(closed)?, closed.session_id],
        )?;
        anyhow::ensure!(changed == 1, "There is no open chapter to close");
        Ok(())
    }

    fn validate_subject_chapter(chapter: &ContextSubjectChapter) -> Result<()> {
        anyhow::ensure!(
            !chapter.id.trim().is_empty() && !chapter.session_id.trim().is_empty(),
            "Subject chapter identity is empty"
        );
        let ordinal = i64::try_from(chapter.ordinal)?;
        let first = i64::try_from(chapter.first_episode_ordinal)?;
        let last = i64::try_from(chapter.last_episode_ordinal)?;
        anyhow::ensure!(
            ordinal > 0 && first > 0 && first <= last,
            "Subject chapter ordinals must be positive and non-regressing"
        );
        match chapter.status {
            SubjectChapterStatus::Open => anyhow::ensure!(
                chapter.description.is_none()
                    && chapter.summary.is_none()
                    && chapter.related_ids.is_empty()
                    && chapter.related_evidence.is_empty()
                    && chapter.model.is_none()
                    && chapter.usage.is_none()
                    && chapter.reasoning.is_none(),
                "Open chapter text, related links and model usage are written only at close"
            ),
            SubjectChapterStatus::Closed => {
                anyhow::ensure!(
                    chapter
                        .description
                        .as_ref()
                        .is_some_and(|text| !text.trim().is_empty()),
                    "Closed chapter description is empty"
                );
                anyhow::ensure!(
                    chapter
                        .summary
                        .as_ref()
                        .is_some_and(|text| !text.trim().is_empty()),
                    "Closed chapter summary must be nonempty"
                );
                anyhow::ensure!(
                    chapter
                        .model
                        .as_ref()
                        .is_some_and(|text| !text.trim().is_empty()),
                    "Closed chapter model is empty"
                );
                anyhow::ensure!(
                    chapter.related_ids.len() == chapter.related_evidence.len(),
                    "Related links must have corresponding evidence"
                );
                let mut seen = std::collections::HashSet::new();
                for (related_id, evidence) in
                    chapter.related_ids.iter().zip(&chapter.related_evidence)
                {
                    anyhow::ensure!(
                        !related_id.trim().is_empty()
                            && !evidence.quote.trim().is_empty()
                            && seen.insert(related_id),
                        "Related links must be unique and have nonempty user evidence"
                    );
                }
            }
        }
        Ok(())
    }

    fn insert_subject_chapter(tx: &Connection, chapter: &ContextSubjectChapter) -> Result<()> {
        tx.execute(
            "INSERT INTO context_subject_chapters(id,session_id,ordinal,first_episode_ordinal,last_episode_ordinal,status,data) VALUES(?1,?2,?3,?4,?5,'open',?6)",
            params![chapter.id, chapter.session_id, i64::try_from(chapter.ordinal)?, i64::try_from(chapter.first_episode_ordinal)?, i64::try_from(chapter.last_episode_ordinal)?, serde_json::to_string(chapter)?],
        )?;
        Ok(())
    }

    fn validate_episode_decision(
        tx: &Connection,
        decision: &ContextDecision,
        episode: &ContextEpisode,
        first: i64,
    ) -> Result<()> {
        anyhow::ensure!(
            !decision.id.trim().is_empty()
                && !decision.model.trim().is_empty()
                && !decision.created_at.trim().is_empty(),
            "Decision identity, model and creation time must be nonempty"
        );
        anyhow::ensure!(
            decision.session_id == episode.session_id
                && decision.episode_ordinal == episode.ordinal,
            "Decision must belong to the inserted episode and session"
        );
        anyhow::ensure!(
            decision.probabilities.len() == decision.option_message_ids.len() + 1,
            "Decision probabilities must cover options and none"
        );
        anyhow::ensure!(
            decision
                .probabilities
                .iter()
                .all(|p| p.is_finite() && (0.0..=1.0).contains(p))
                && (decision.probabilities.iter().sum::<f64>() - 1.0).abs() <= 1e-6,
            "Decision probabilities must be a finite distribution"
        );
        anyhow::ensure!(
            decision
                .choice
                .as_ref()
                .is_none_or(|choice| decision.option_message_ids.contains(choice)),
            "Decision choice was not offered"
        );
        let mut previous = None;
        for message_id in &decision.option_message_ids {
            let seq = Self::session_message_seq(tx, &episode.session_id, message_id)?;
            let message: String =
                tx.query_row("SELECT data FROM messages WHERE seq=?1", [seq], |row| {
                    row.get(0)
                })?;
            anyhow::ensure!(
                first <= seq
                    && previous.is_none_or(|previous| seq > previous)
                    && (is_typed_user(&Self::message_from_data(tx, &message)?)
                        || (decision.option_message_ids.len() == 1 && seq == first)),
                "Decision options must be ordered typed requests or the single current episode boundary in the same session"
            );
            previous = Some(seq);
        }
        Ok(())
    }
}
