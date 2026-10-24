//! Fork selection keeps capture provenance paired with the exact immutable request.
use super::*;
use crate::episode_models::{ChapterInput, ChapterResult, EpisodeFork};

impl EpisodeWorker {
    pub(super) fn context_fork(
        &self,
        session: &str,
        first: &str,
        last: &str,
    ) -> Result<Option<(EpisodeFork, Vec<usize>)>> {
        Ok(if self.config.summary_input == EpisodeSummaryInput::Fork {
            match self
                .estimator
                .episode_fork_with_context(session, first, last)
            {
                Ok(Some(fork)) => Some(fork),
                Ok(None) => {
                    self.models
                        .record_fork_fallback(session, "capture_missing_or_source_incompatible");
                    anyhow::bail!(
                        "Episode fork capture is not ready; canonical source remains uncovered"
                    )
                }
                Err(error) => {
                    self.models
                        .record_fork_fallback(session, "canonical_source_unavailable");
                    return Err(error.context(
                        "Episode fork source is unavailable; canonical source remains uncovered",
                    ));
                }
            }
        } else {
            None
        })
    }

    /// Close a chapter as a fork of the latest confirmed main capture, cut at or
    /// extended to the chapter's last message. Without one the call is fresh.
    pub(super) async fn chapter_model(
        &self,
        session: &str,
        last: &str,
        input: ChapterInput,
    ) -> Result<ChapterResult> {
        // A capture that cannot be read only costs the cache; the call stays fresh.
        let fork = self.estimator.chapter_fork(session, last).ok().flatten();
        let call = async {
            match fork {
                Some((fork, cold)) => {
                    self.models
                        .record_fork_context(session, &fork.request, &cold);
                    self.models.close_chapter_fork(input, fork).await
                }
                None => self.models.close_chapter(input).await,
            }
        };
        self.counted(session, RequestKind::SummaryFork, call).await
    }
}
