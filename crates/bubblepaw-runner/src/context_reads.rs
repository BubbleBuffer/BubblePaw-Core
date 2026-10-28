//! Read-only context inspection shared by SDK and transport hosts.
use crate::summarization::canonical_message_tokens;
use anyhow::{Context, Result, bail, ensure};
use bubblepaw_core::{
    ContextEvent, ContextView, Message, MessageTokenSource, MessageTokens, Store,
    SubjectChapterStatus,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fmt, ops::RangeInclusive};

pub const HISTORY_RANGE_DEFAULT: usize = 200;
pub const HISTORY_RANGE_MAX: usize = 1000;

/// A malformed context read, distinct from storage failures for transport mapping.
#[derive(Debug)]
pub struct InvalidContextRead(pub String);
impl fmt::Display for InvalidContextRead {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        output.write_str(&self.0)
    }
}
impl std::error::Error for InvalidContextRead {}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    InvalidContextRead(message.into()).into()
}

fn summary_tokens(text: &str) -> u64 {
    crate::episodes::text_tokens(text) as u64
}

#[derive(Clone, Debug, Serialize)]
pub struct EpisodeMetadata {
    pub id: String,
    pub ordinal: usize,
    pub chapter_id: String,
    pub first_message_id: String,
    pub last_message_id: String,
    pub title: String,
    pub status: &'static str,
    pub source_tokens: u64,
    pub summary_tokens: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChapterMetadata {
    pub id: String,
    pub ordinal: usize,
    pub first_episode_ordinal: usize,
    pub last_episode_ordinal: usize,
    pub first_message_id: Option<String>,
    pub last_message_id: Option<String>,
    pub title: Option<String>,
    pub status: SubjectChapterStatus,
    pub source_tokens: u64,
    pub summary_tokens: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct LegacyChapterMetadata {
    pub id: String,
    pub ordinal: usize,
    pub first_message_id: String,
    pub last_message_id: String,
    pub title: String,
    pub status: &'static str,
    pub source_tokens: Option<u64>,
    pub summary_tokens: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ContextStructure {
    pub episodes: Vec<EpisodeMetadata>,
    pub chapters: Vec<ChapterMetadata>,
    pub legacy_chapters: Vec<LegacyChapterMetadata>,
    pub summary_tokens_estimated: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryKind {
    Episode,
    Chapter,
}

#[derive(Clone, Debug, Serialize)]
pub struct ContextSummary {
    pub kind: SummaryKind,
    pub id: String,
    pub summary: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct HistoryRange {
    pub first_seq: Option<i64>,
    pub last_seq: Option<i64>,
    pub first_message_id: Option<String>,
    pub last_message_id: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, Serialize)]
pub struct HistoryMessage {
    pub seq: i64,
    pub message: Message,
    pub tokens: u64,
    pub source: MessageTokenSource,
}

#[derive(Clone, Debug, Serialize)]
pub struct HistoryRangePage {
    pub messages: Vec<HistoryMessage>,
    pub next_seq: Option<i64>,
}

pub fn view(store: &Store, session: &str) -> Result<Option<ContextView>> {
    store.session(session)?;
    store.context_view(session)
}

pub fn events(store: &Store, session: &str) -> Result<Vec<ContextEvent>> {
    store.session(session)?;
    store.context_events(session)
}

/// Derived metadata only: summary bodies and canonical source messages stay out.
pub fn structure(store: &Store, session: &str) -> Result<ContextStructure> {
    // A gather commits its episode and chapter together. Keep separate metadata
    // reads on one stable revision so their ranges and source totals agree.
    for _ in 0..3 {
        let revision = store.revision();
        let metadata = structure_once(store, session);
        if store.revision() == revision {
            return metadata;
        }
    }
    bail!("Context structure changed during read; retry")
}

fn structure_once(store: &Store, session: &str) -> Result<ContextStructure> {
    store.session(session)?;
    let episodes = store.context_episodes(session)?;
    let chapters = store
        .context_subject_chapters(session)?
        .into_iter()
        .map(|chapter| {
            let members: Vec<_> = episodes
                .iter()
                .filter(|e| e.chapter_id == chapter.id)
                .collect();
            ChapterMetadata {
                first_message_id: members.first().map(|e| e.first_message_id.clone()),
                last_message_id: members.last().map(|e| e.last_message_id.clone()),
                source_tokens: members.iter().fold(0_u64, |total, e| {
                    total.saturating_add(e.source_tokens as u64)
                }),
                summary_tokens: chapter.summary.as_deref().map_or(0, summary_tokens),
                id: chapter.id,
                ordinal: chapter.ordinal,
                first_episode_ordinal: chapter.first_episode_ordinal,
                last_episode_ordinal: chapter.last_episode_ordinal,
                title: chapter.description,
                status: chapter.status,
            }
        })
        .collect();
    let legacy_chapters = store
        .context_chapters(session)?
        .into_iter()
        .map(|chapter| LegacyChapterMetadata {
            summary_tokens: summary_tokens(&chapter.summary),
            id: chapter.id,
            ordinal: chapter.ordinal,
            first_message_id: chapter.first_message_id,
            last_message_id: chapter.last_message_id,
            title: chapter.title,
            status: "closed",
            source_tokens: None,
        })
        .collect();
    Ok(ContextStructure {
        episodes: episodes
            .into_iter()
            .map(|episode| EpisodeMetadata {
                summary_tokens: summary_tokens(&episode.summary),
                id: episode.id,
                ordinal: episode.ordinal,
                chapter_id: episode.chapter_id,
                first_message_id: episode.first_message_id,
                last_message_id: episode.last_message_id,
                title: episode.title,
                status: "written",
                source_tokens: episode.source_tokens as u64,
            })
            .collect(),
        chapters,
        legacy_chapters,
        summary_tokens_estimated: true,
    })
}

pub fn summary(
    store: &Store,
    session: &str,
    kind: SummaryKind,
    id: &str,
) -> Result<ContextSummary> {
    store.session(session)?;
    ensure!(
        !id.trim().is_empty(),
        invalid("summary id must be nonempty")
    );
    let body = match kind {
        SummaryKind::Episode => Some(
            store
                .context_episodes(session)?
                .into_iter()
                .find(|episode| episode.id == id)
                .context("unknown or expired context summary")?
                .summary,
        ),
        SummaryKind::Chapter => {
            if let Some(chapter) = store
                .context_subject_chapters(session)?
                .into_iter()
                .find(|chapter| chapter.id == id)
            {
                chapter.summary
            } else {
                Some(
                    store
                        .context_chapters(session)?
                        .into_iter()
                        .find(|chapter| chapter.id == id)
                        .context("unknown or expired context summary")?
                        .summary,
                )
            }
        }
    };
    Ok(ContextSummary {
        kind,
        id: id.into(),
        summary: body,
    })
}

impl HistoryRange {
    fn bounds(&self, store: &Store, session: &str) -> Result<(i64, i64, usize)> {
        let limit = self.limit.unwrap_or(HISTORY_RANGE_DEFAULT);
        ensure!(
            (1..=HISTORY_RANGE_MAX).contains(&limit),
            invalid(format!("limit must be between 1 and {HISTORY_RANGE_MAX}"))
        );
        let (first, last) = match (
            self.first_seq,
            self.last_seq,
            self.first_message_id.as_deref(),
            self.last_message_id.as_deref(),
        ) {
            (Some(first), Some(last), None, None) => (first, last),
            (None, None, Some(first), Some(last)) => (
                range_message_seq(store, session, first, "first_message_id")?,
                range_message_seq(store, session, last, "last_message_id")?,
            ),
            _ => bail!(invalid(
                "provide exactly first_seq/last_seq or first_message_id/last_message_id"
            )),
        };
        ensure!(
            first >= 0 && last >= first,
            invalid("range must be nonnegative and ordered")
        );
        Ok((first, last, limit))
    }
}

fn range_message_seq(store: &Store, session: &str, id: &str, field: &str) -> Result<i64> {
    store.message_seq(session, id).map_err(|error| {
        if error.to_string() == "Message does not belong to the session"
            || error
                .downcast_ref::<rusqlite::Error>()
                .is_some_and(|error| matches!(error, rusqlite::Error::QueryReturnedNoRows))
        {
            invalid(format!("{field} does not belong to this session"))
        } else {
            error
        }
    })
}

pub fn history_range(
    store: &Store,
    session: &str,
    range: &HistoryRange,
) -> Result<HistoryRangePage> {
    store.session(session)?;
    let (first, last, limit) = range.bounds(store, session)?;
    let mut rows = store.messages_range(session, first, last, limit + 1)?;
    let next_seq = rows.get(limit).map(|(seq, _)| *seq);
    rows.truncate(limit);
    let mut messages = Vec::with_capacity(rows.len());
    if let Some((end, _)) = rows.last() {
        let reported = store.reported_message_tokens(session, first..=*end)?;
        ensure!(
            reported.len() == rows.len(),
            "Canonical range changed during token read"
        );
        for ((seq, message), count) in rows.into_iter().zip(reported) {
            ensure!(
                message.id == count.message_id,
                "Canonical range changed during token read"
            );
            let (tokens, source) = count.tokens.map_or_else(
                || {
                    (
                        canonical_message_tokens(&message) as u64,
                        MessageTokenSource::Estimated,
                    )
                },
                |tokens| (tokens, MessageTokenSource::Reported),
            );
            messages.push(HistoryMessage {
                seq,
                message,
                tokens,
                source,
            });
        }
    }
    Ok(HistoryRangePage { messages, next_seq })
}

/// Read chronological counts, decoding only contiguous missing-report ranges.
pub fn message_tokens(
    store: &Store,
    session_id: &str,
    range: RangeInclusive<i64>,
) -> Result<Vec<MessageTokens>> {
    let reported = store.reported_message_tokens(session_id, range)?;
    let mut counts = Vec::with_capacity(reported.len());
    let mut index = 0;
    while index < reported.len() {
        if let Some(tokens) = reported[index].tokens {
            counts.push(MessageTokens {
                message_id: reported[index].message_id.clone(),
                tokens,
                source: MessageTokenSource::Reported,
            });
            index += 1;
            continue;
        }
        let start = index;
        while index < reported.len() && reported[index].tokens.is_none() {
            index += 1;
        }
        let first = store.message_seq(session_id, &reported[start].message_id)?;
        let last = store.message_seq(session_id, &reported[index - 1].message_id)?;
        let messages = store.messages_range(session_id, first, last, index - start)?;
        ensure!(
            messages.len() == index - start,
            "Canonical messages disappeared during token read"
        );
        for ((_, message), expected) in messages.into_iter().zip(&reported[start..index]) {
            ensure!(
                message.id == expected.message_id,
                "Canonical message range changed during token read"
            );
            counts.push(MessageTokens {
                tokens: canonical_message_tokens(&message) as u64,
                message_id: message.id,
                source: MessageTokenSource::Estimated,
            });
        }
    }
    Ok(counts)
}

/// Transport-neutral payload mapping; hosts enforce their own session visibility.
pub fn read(store: &Store, session: &str, method: &str, params: &Value) -> Result<Value> {
    Ok(match method {
        "context.view" => json!({"view":view(store, session)?}),
        "context.structure" => serde_json::to_value(structure(store, session)?)?,
        "context.summary" => {
            let kind = serde_json::from_value(params["kind"].clone())
                .map_err(|error| invalid(error.to_string()))?;
            let id = params["id"]
                .as_str()
                .ok_or_else(|| invalid("summary id is required"))?;
            serde_json::to_value(summary(store, session, kind, id)?)?
        }
        "context.events" => json!({"events":events(store, session)?}),
        "history.range" => {
            let range = serde_json::from_value(params.clone())
                .map_err(|error| invalid(error.to_string()))?;
            serde_json::to_value(history_range(store, session, &range)?)?
        }
        _ => bail!("unknown method: {method}"),
    })
}
