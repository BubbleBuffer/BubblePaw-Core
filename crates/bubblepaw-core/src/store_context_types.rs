//! Context chapter, episode and decision record types.
use super::*;

/// An immutable summary of a contiguous range of canonical session messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextChapter {
    pub id: String,
    pub session_id: String,
    /// One-based append order within this session.
    pub ordinal: usize,
    pub first_message_id: String,
    pub last_message_id: String,
    pub title: String,
    pub summary: String,
    pub model: String,
    pub usage: Option<Usage>,
    pub created_at: String,
}

/// A write-once summary of a consecutive range of canonical messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextEpisode {
    pub id: String,
    pub session_id: String,
    pub ordinal: usize,
    pub first_message_id: String,
    pub last_message_id: String,
    pub source_tokens: usize,
    /// A summary length target, not an enforced output limit.
    pub cap: usize,
    pub origin_request_id: Option<String>,
    pub title: String,
    pub summary: String,
    pub model: String,
    pub usage: Option<Usage>,
    #[serde(default)]
    pub reasoning: Option<String>,
    pub created_at: String,
    pub chapter_id: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubjectChapterStatus {
    Open,
    Closed,
}

/// Exact request or delegated-task words offered for a related chapter link.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelatedEvidence {
    pub quote: String,
    #[serde(default)]
    pub related_quote_matched: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<crate::MessageOrigin>,
}

impl From<String> for RelatedEvidence {
    fn from(quote: String) -> Self {
        Self {
            quote,
            related_quote_matched: None,
            origin: None,
        }
    }
}

impl From<&str> for RelatedEvidence {
    fn from(quote: &str) -> Self {
        Self::from(quote.to_owned())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    Split,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextDecision {
    pub id: String,
    pub session_id: String,
    pub kind: DecisionKind,
    pub episode_ordinal: usize,
    pub option_message_ids: Vec<String>,
    /// In option order, followed by the probability of choosing none.
    pub probabilities: Vec<f64>,
    pub choice: Option<String>,
    pub model: String,
    pub usage: Option<Usage>,
    #[serde(default)]
    pub reasoning: Option<String>,
    pub created_at: String,
}

/// One episode and its chapter transition, committed together or not at all.
#[derive(Debug, Clone)]
pub struct EpisodeBatch {
    pub episode: ContextEpisode,
    pub decisions: Vec<ContextDecision>,
    /// The open chapter membership after appending this episode, before any close.
    pub open_chapter: ContextSubjectChapter,
    /// The previous open chapter's final metadata, when starting a new chapter.
    pub closed_chapter: Option<ContextSubjectChapter>,
    /// Final metadata for closing this episode's chapter after inserting it.
    /// Cannot be combined with `closed_chapter`.
    pub close_after_episode: Option<ContextSubjectChapter>,
    /// Last covered message ID observed by the caller; None means no coverage.
    pub expected_previous_coverage: Option<String>,
}
