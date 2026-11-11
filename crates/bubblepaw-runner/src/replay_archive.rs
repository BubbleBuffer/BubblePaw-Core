//! Archive normalization: recorded tool calls without outcomes get an explicit unknown result.
use super::*;

pub(super) const ARCHIVE_UNKNOWN_OUTCOME: &str =
    "[outcome not recorded in source archive; unknown]";
pub(super) const ARCHIVE_NORMALIZATION_POLICY: &str =
    "archive-unknown-outcome-v1-global-recorded-results-before-continuation";

#[derive(Clone, Default)]
pub(super) struct ArchiveNormalization {
    pub(super) source_messages_total: usize,
    pub(super) source_corpus_sha256: String,
    pub(super) replay_corpus_sha256: String,
    // One-based canonical positions; heartbeat counts need no history/SQL reads.
    pub(super) marker_steps: Vec<usize>,
}
impl ArchiveNormalization {
    pub(super) fn markers_replayed(&self, canonical_messages: usize) -> usize {
        self.marker_steps
            .partition_point(|step| *step <= canonical_messages)
    }
    pub(super) fn metadata(&self) -> Value {
        json!({"policy":ARCHIVE_NORMALIZATION_POLICY,"origin":"archive_unknown_outcome","source_corpus_sha256":self.source_corpus_sha256,"replay_corpus_sha256":self.replay_corpus_sha256,"position_basis":"Canonical source-plus-marker steps; typed-user turn numbers and original message IDs remain unchanged","placement":"After recorded round results, before the next non-tool continuation, or at EOF; only calls with no recorded result anywhere in the source archive","is_error":false})
    }
}

/// Close only genuinely absent archive outcomes, without executing or guessing tools.
/// A whole-source result-ID scan prevents inventing a marker for a delayed result.
pub(super) fn normalize_archive(corpus: &mut ReplayCorpus) -> Result<ArchiveNormalization> {
    let input = corpus.messages.clone();
    let had_markers = input
        .iter()
        .any(|message| message.origin == Some(MessageOrigin::ArchiveUnknownOutcome));
    let source: Vec<_> = input
        .iter()
        .filter(|message| message.origin != Some(MessageOrigin::ArchiveUnknownOutcome))
        .cloned()
        .collect();
    let mut message_ids = HashSet::new();
    let mut call_ids = HashSet::new();
    let mut result_ids = HashSet::new();
    for message in &source {
        ensure!(
            !message.id.is_empty() && message_ids.insert(message.id.clone()),
            "Recorded chat contains empty or duplicate message IDs"
        );
        for call in &message.tool_calls {
            ensure!(
                message.role == Role::Assistant
                    && !call.id.is_empty()
                    && call_ids.insert(call.id.clone()),
                "Recorded chat contains ambiguous or duplicate tool call IDs"
            );
        }
        if let Some(call_id) = &message.tool_call_id {
            ensure!(
                message.role == Role::Tool
                    && !call_id.is_empty()
                    && result_ids.insert(call_id.clone()),
                "Recorded chat contains ambiguous or duplicate tool results"
            );
        }
    }
    corpus.messages = source;
    let source_corpus_sha256 = crate::probes::fingerprint(corpus)?;
    let source_messages_total = corpus.messages.len();
    let mut normalized = Vec::with_capacity(source_messages_total);
    let mut pending = Vec::new();
    let mut marker_steps = Vec::new();
    let append_markers = |normalized: &mut Vec<Message>,
                          pending: &mut Vec<Message>,
                          marker_steps: &mut Vec<usize>| {
        for marker in pending.drain(..) {
            normalized.push(marker);
            marker_steps.push(normalized.len());
        }
    };
    for message in &corpus.messages {
        if message.role != Role::Tool {
            append_markers(&mut normalized, &mut pending, &mut marker_steps);
        }
        normalized.push(message.clone());
        for call in &message.tool_calls {
            if result_ids.contains(&call.id) {
                continue;
            }
            let id = format!(
                "archive-unknown-{}",
                crate::probes::fingerprint(&(ARCHIVE_NORMALIZATION_POLICY, &message.id, &call.id))?
            );
            ensure!(
                message_ids.insert(id.clone()),
                "Archive unknown-outcome marker ID collides with a source message"
            );
            pending.push(Message {
                id,
                session_id: message.session_id.clone(),
                role: Role::Tool,
                origin: Some(MessageOrigin::ArchiveUnknownOutcome),
                content: ARCHIVE_UNKNOWN_OUTCOME.into(),
                tool_calls: vec![],
                tool_call_id: Some(call.id.clone()),
                provider_state: None,
                is_error: false,
                created_at: message.created_at.clone(),
            });
        }
    }
    append_markers(&mut normalized, &mut pending, &mut marker_steps);
    if had_markers {
        ensure!(
            serde_json::to_value(&input)? == serde_json::to_value(&normalized)?,
            "Existing archive unknown-outcome markers differ from deterministic normalization"
        );
    }
    corpus.messages = normalized;
    let replay_corpus_sha256 =
        crate::probes::fingerprint(&(ARCHIVE_NORMALIZATION_POLICY, &*corpus))?;
    Ok(ArchiveNormalization {
        source_messages_total,
        source_corpus_sha256,
        replay_corpus_sha256,
        marker_steps,
    })
}

/// Apply the same read-only archive normalization used by recorded replay.
pub(crate) fn normalize_corpus(corpus: &mut ReplayCorpus) -> Result<()> {
    normalize_archive(corpus)?;
    Ok(())
}
