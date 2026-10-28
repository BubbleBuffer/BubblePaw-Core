//! Persisted subject chapter metadata.
use super::*;

/// A named contiguous range of actual chapter episode ordinals.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChapterSection {
    pub name: String,
    pub first_episode: usize,
    pub last_episode: usize,
}

/// Consecutive episode membership. Text and related links are set only at close.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSubjectChapter {
    pub id: String,
    pub session_id: String,
    pub ordinal: usize,
    pub first_episode_ordinal: usize,
    pub last_episode_ordinal: usize,
    pub status: SubjectChapterStatus,
    pub description: Option<String>,
    pub summary: Option<String>,
    /// Structured close-time retrieval hints; absent for v1 or invalid v2 output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decisions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sections: Option<Vec<ChapterSection>>,
    pub related_ids: Vec<String>,
    /// The user's words supporting each corresponding related ID.
    pub related_evidence: Vec<RelatedEvidence>,
    pub model: Option<String>,
    pub usage: Option<Usage>,
    #[serde(default)]
    pub reasoning: Option<String>,
}

impl ContextSubjectChapter {
    /// Stable within a session because chapter ordinals never change.
    pub fn short_id(&self) -> String {
        format!("ch-{}", self.ordinal)
    }
}

impl ContextSubjectChapter {
    pub(super) fn validate_retrieval_metadata(&self) -> Result<()> {
        let (decisions, sections) = match (&self.decisions, &self.sections) {
            (None, None) => return Ok(()),
            (Some(decisions), Some(sections)) => (decisions, sections),
            _ => bail!("Chapter decisions and sections must be present together"),
        };
        anyhow::ensure!(
            self.status == SubjectChapterStatus::Closed,
            "Chapter retrieval metadata is written only at close"
        );
        anyhow::ensure!(
            decisions.iter().all(|decision| !decision.trim().is_empty()
                && decision.chars().count() <= 100
                && !decision.contains(['\r', '\n'])),
            "Chapter decisions must be nonempty single lines of at most 100 characters"
        );
        anyhow::ensure!(
            !sections.is_empty(),
            "Chapter sections must cover every episode"
        );
        let mut next = Some(self.first_episode_ordinal);
        for section in sections {
            anyhow::ensure!(
                (1..=5).contains(&section.name.split_whitespace().count())
                    && section.name == section.name.trim()
                    && !section.name.contains(['|', '\r', '\n']),
                "Chapter section names must contain one to five words"
            );
            anyhow::ensure!(
                Some(section.first_episode) == next
                    && section.first_episode <= section.last_episode
                    && section.last_episode <= self.last_episode_ordinal,
                "Chapter sections must tile chapter episodes in order"
            );
            next = section.last_episode.checked_add(1);
        }
        anyhow::ensure!(
            sections
                .last()
                .is_some_and(|section| section.last_episode == self.last_episode_ordinal),
            "Chapter sections must cover every episode"
        );
        Ok(())
    }
}

#[cfg(test)]
#[path = "store_chapters_tests.rs"]
mod tests;
