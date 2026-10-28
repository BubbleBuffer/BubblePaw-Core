//! Durable facts about context compaction and read-time message token counts.
use crate::{Message, Usage};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextEventKind {
    EpisodeWritten,
    ChapterClosed,
    Applied,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextTrigger {
    Pressure,
    Emergency,
    SourceSize,
    SubjectChange,
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextEvent {
    pub id: String,
    pub session_id: String,
    pub kind: ContextEventKind,
    pub created_at: String,
    pub trigger: ContextTrigger,
    pub trigger_text: String,
    pub first_message_id: String,
    pub last_message_id: String,
    pub episode_ids: Vec<String>,
    pub summary_id: Option<String>,
    /// Covered original source tokens, or None when inherited coverage has no stored count.
    pub tokens_before: Option<u64>,
    pub tokens_after: u64,
    pub input_before: Option<u64>,
    pub input_after: Option<u64>,
    pub input_estimated: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextBlockKind {
    Opening,
    MemoryIndex,
    Anchors,
    ChapterSummary,
    EpisodeSummary,
    RawSuffix,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextBlock {
    pub kind: ContextBlockKind,
    pub tokens: u64,
    pub id: Option<String>,
    pub first_message_id: Option<String>,
    pub last_message_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageTokenSource {
    Reported,
    Estimated,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessageTokens {
    pub message_id: String,
    pub tokens: u64,
    pub source: MessageTokenSource,
}

/// Provider output counts for canonical messages; None means no complete report.
/// Hosts may estimate missing values with their chosen tokenizer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReportedMessageTokens {
    pub message_id: String,
    pub tokens: Option<u64>,
}

/// A host's disposable serving summary. Canonical messages remain in the archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextCheckpoint {
    pub id: String,
    pub session_id: String,
    pub through_message_id: String,
    pub model: String,
    pub summary: String,
    /// Deterministic file trail retained by a rolling summary host.
    #[serde(default)]
    pub files_touched: Vec<String>,
    pub created_at: String,
    pub usage: Option<Usage>,
}

/// A frozen, disposable serving view; canonical history remains unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextView {
    pub session_id: String,
    /// Inclusive canonical messages(seq) boundary, or None for a fully raw view.
    pub frontier: Option<i64>,
    /// Instructions frozen with the rendered message prefix until compaction.
    #[serde(default)]
    pub system_prompt: String,
    pub prefix: Vec<Message>,
    /// Canonical output IDs masked in the raw zone.
    pub masked_tool_output_ids: Vec<String>,
    /// Call IDs clipped in the raw zone.
    pub clipped_tool_call_ids: Vec<String>,
    pub created_at: String,
    pub threshold: usize,
    pub budget: usize,
    pub tokens: usize,
    /// Estimated token contributions in serving order. Absent in historical views.
    #[serde(default)]
    pub blocks: Vec<ContextBlock>,
    #[serde(default)]
    pub blocks_estimated: bool,
}
