//! Indexed cold-history access. Metadata may grow with history; decoded bodies do not.
use anyhow::{Context, Result};
use bubblepaw_core::{Message, MessageOrigin, Role, Store, is_typed_user};
use std::{cell::Cell, collections::HashMap, sync::Mutex};

use crate::summarization::SummarizingContext;

pub(crate) struct HistorySnapshot {
    store: Store,
    session: String,
    pub metadata: Vec<Message>,
    sequences: Vec<i64>,
    positions: HashMap<String, usize>,
    tokens: Mutex<TokenCounts>,
}

struct TokenCounts {
    counts: Vec<usize>,
    first_known: usize,
}

impl HistorySnapshot {
    pub fn index(store: &Store, session: &str) -> Result<Self> {
        let mut metadata = Vec::new();
        let mut sequences = Vec::new();
        let mut positions = HashMap::new();
        store.for_each_message_header(session, |seq, message| {
            positions
                .entry(message.id.clone())
                .or_insert(metadata.len());
            sequences.push(seq);
            metadata.push(message);
            Ok(())
        })?;
        let tokens = Mutex::new(TokenCounts {
            counts: vec![0; metadata.len()],
            first_known: metadata.len(),
        });
        let mut snapshot = Self {
            store: store.clone(),
            session: session.into(),
            metadata,
            sequences,
            positions,
            tokens,
        };
        if let Some(frontier) = store.imported_history_seed(session)?.0 {
            let end = snapshot
                .sequences
                .iter()
                .position(|seq| *seq == frontier)
                .context("Imported boundary is missing from canonical history")?
                + 1;
            snapshot.seed_complete_prefix(end);
        }
        Ok(snapshot)
    }

    pub fn position(&self, id: &str) -> Result<usize> {
        self.positions
            .get(id)
            .copied()
            .context("Saved context cutoff is missing from canonical history")
    }

    pub fn seq(&self, index: usize) -> i64 {
        self.sequences[index]
    }

    pub fn message(&self, index: usize) -> Result<Message> {
        self.store
            .messages_range(&self.session, self.seq(index), self.seq(index), 1)?
            .into_iter()
            .next()
            .map(|(_, message)| message)
            .context("Indexed canonical message disappeared")
    }

    pub fn walk(
        &self,
        start: usize,
        end: usize,
        mut visit: impl FnMut(usize, Message) -> Result<()>,
    ) -> Result<()> {
        if start >= end {
            return Ok(());
        }
        let mut index = start;
        self.store.for_each_message_range(
            &self.session,
            self.seq(start),
            self.seq(end - 1),
            |_, message| {
                visit(index, message)?;
                index += 1;
                Ok(())
            },
        )?;
        Ok(())
    }

    /// Keep identities, roles, dates and call boundaries, never body/argument payloads.
    fn strip(mut message: Message) -> Message {
        // Legacy generated users are identified by their original envelope.
        // Preserve that classification before dropping its content.
        if message.role == Role::User && message.origin.is_none() {
            message.origin = Some(if is_typed_user(&message) {
                MessageOrigin::Typed
            } else {
                MessageOrigin::Generated
            });
        }
        message.content = String::new();
        message.provider_state = None;
        for call in &mut message.tool_calls {
            call.name = String::new();
            call.arguments = serde_json::Value::Null;
        }
        message
    }

    pub fn scan(&mut self, start: usize, estimator: &SummarizingContext) -> Result<()> {
        self.scan_range(start, self.metadata.len(), estimator)
    }

    pub fn scan_range(
        &mut self,
        start: usize,
        end: usize,
        estimator: &SummarizingContext,
    ) -> Result<()> {
        if start >= end {
            return Ok(());
        }
        let mut index = start;
        let mut tokens = self.tokens.lock().unwrap();
        self.store.for_each_message_range(
            &self.session,
            self.seq(start),
            self.seq(end - 1),
            |_, message| {
                tokens.counts[index] = estimator.source_message_tokens(&message);
                self.metadata[index] = Self::strip(message);
                index += 1;
                Ok(())
            },
        )?;
        if end >= tokens.first_known {
            tokens.first_known = tokens.first_known.min(start);
        }
        Ok(())
    }

    pub fn tail_start(&self, budget: usize) -> usize {
        let mut tokens = 0;
        let counts = self.tokens.lock().unwrap();
        for index in (counts.first_known..counts.counts.len()).rev() {
            tokens += counts.counts[index];
            if tokens >= budget {
                return index;
            }
        }
        counts.first_known
    }

    #[cfg(test)]
    pub fn plan(&self, coverage_end: usize) -> Result<crate::episodes::EpisodePlan> {
        self.plan_with_config(coverage_end, &crate::episodes::EpisodeConfig::default())
    }

    pub fn plan_with_config(
        &self,
        coverage_end: usize,
        config: &crate::episodes::EpisodeConfig,
    ) -> Result<crate::episodes::EpisodePlan> {
        let next = Cell::new(coverage_end);
        let counts = self.tokens.lock().unwrap();
        crate::episodes::plan_episodes_with_config(
            &self.metadata,
            coverage_end,
            |_| {
                let index = next.get();
                next.set(index + 1);
                counts.counts[index]
            },
            is_typed_user,
            config,
        )
    }

    pub fn manual_episode(
        &self,
        start: usize,
        end: usize,
    ) -> Option<crate::episodes::PlannedEpisode> {
        let next = Cell::new(start);
        let counts = self.tokens.lock().unwrap();
        crate::episodes::manual_episode(&self.metadata, start, end, |_| {
            let index = next.get();
            next.set(index + 1);
            counts.counts[index]
        })
    }

    /// The serving boundary ends the prior context's pending-call scope. This
    /// affects planning metadata only; canonical call IDs remain registered.
    pub fn seed_complete_prefix(&mut self, end: usize) {
        for message in &mut self.metadata[..end] {
            message.tool_calls.clear();
            message.tool_call_id = None;
        }
    }

    /// The isolated request anchor is the only original body a summary prefix needs.
    pub fn anchor_history(&self, raw_start: usize) -> Result<Vec<Message>> {
        let mut history = self.metadata.clone();
        if let Some(index) = self
            .metadata
            .iter()
            .position(crate::episodes::is_task_anchor)
            && index < raw_start
        {
            history[index] = self.message(index)?;
        }
        if let Some(index) = self
            .metadata
            .iter()
            .position(|m| m.origin == Some(MessageOrigin::ChildTask))
            && index < raw_start
        {
            history[index] = self.message(index)?;
        }
        if let Some(index) = self.metadata.iter().rposition(is_typed_user)
            && index < raw_start
        {
            history[index] = self.message(index)?;
        }
        if let Some(index) = self
            .metadata
            .iter()
            .rposition(crate::episodes::is_task_anchor)
            && index < raw_start
        {
            history[index] = self.message(index)?;
        }
        Ok(history)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        episodes::plan_episodes_with_estimator, provider::ScriptedProvider,
        summarization::SummaryConfig,
    };
    use bubblepaw_core::ToolCall;
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn stripped_planning_retains_rounds_origins_global_indices_and_duplicate_id_tokens()
    -> Result<()> {
        let store = Store::in_memory()?;
        let session = store.create_session("metadata planning")?;
        let estimator = SummarizingContext::new(
            store.clone(),
            Arc::new(ScriptedProvider::new("unused")),
            SummaryConfig::default(),
            None,
        )?;
        let mut history = Vec::new();
        for (index, role) in [
            Role::User,
            Role::Assistant,
            Role::User,
            Role::Assistant,
            Role::Tool,
            Role::User,
            Role::Assistant,
            Role::User,
        ]
        .into_iter()
        .enumerate()
        {
            history.push(Message {
                id: format!("message-{index}"),
                session_id: session.id.clone(),
                role: role.clone(),
                content: if role == Role::User {
                    format!("request {index}")
                } else {
                    " datum".repeat(if index == 1 { 17_000 } else { 33_000 })
                },
                origin: None,
                tool_calls: Vec::new(),
                tool_call_id: None,
                provider_state: None,
                is_error: false,
                created_at: "2026-10-12T00:00:00Z".into(),
            });
        }
        history[2].content = "Task:\nGenerated child task".into();
        history[3].id = history[1].id.clone();
        history[3].tool_calls.push(ToolCall {
            id: "pending".into(),
            name: "read".into(),
            arguments: json!({"path":"secret-shaped-synthetic","body":"argument".repeat(2000)}),
        });
        history[4].tool_call_id = Some("pending".into());
        store.append_batch(&history)?;
        let mut snapshot = HistorySnapshot::index(&store, &session.id)?;
        snapshot.scan(0, &estimator)?;
        assert!(!is_typed_user(&snapshot.metadata[2]));
        for coverage in [0, 2, 5] {
            let old = plan_episodes_with_estimator(&history, coverage, &estimator)?;
            let new = snapshot.plan(coverage)?;
            assert_eq!(old.remainder, new.remainder);
            assert_eq!(old.episodes.len(), new.episodes.len());
            for (old, new) in old.episodes.iter().zip(&new.episodes) {
                assert_eq!(old.message_range, new.message_range);
                assert_eq!(old.kind, new.kind);
                assert_eq!(old.source_tokens, new.source_tokens);
                assert_eq!(old.origin_request_id, new.origin_request_id);
                assert_eq!(old.typed_user_message_ids, new.typed_user_message_ids);
            }
        }
        let mut complete = HistorySnapshot::index(&store, &session.id)?;
        complete.scan(5, &estimator)?;
        let expected = snapshot.plan(5)?;
        let actual = complete.plan(5)?;
        assert_eq!(expected.remainder, actual.remainder);
        assert_eq!(
            expected.episodes[0].message_range,
            actual.episodes[0].message_range
        );
        assert_eq!(
            expected.episodes[0].origin_request_id,
            actual.episodes[0].origin_request_id
        );
        Ok(())
    }

    #[test]
    fn legacy_pending_call_metadata_keeps_typed_interrupt_rejection() -> Result<()> {
        let store = Store::in_memory()?;
        let session = store.create_session("legacy pending")?;
        let estimator = SummarizingContext::new(
            store.clone(),
            Arc::new(ScriptedProvider::new("unused")),
            SummaryConfig::default(),
            None,
        )?;
        let mut history = vec![Message {
            id: "call-message".into(),
            session_id: session.id.clone(),
            role: Role::Assistant,
            content: "old call".into(),
            origin: None,
            tool_calls: vec![ToolCall {
                id: "unknown-effect".into(),
                name: "read".into(),
                arguments: json!({"path":"a"}),
            }],
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: "2026-10-12T00:00:00Z".into(),
        }];
        history.push(Message {
            id: "interrupt".into(),
            role: Role::User,
            content: "new task".into(),
            tool_calls: Vec::new(),
            ..history[0].clone()
        });
        store.append_batch(&history)?;
        let mut snapshot = HistorySnapshot::index(&store, &session.id)?;
        snapshot.scan(0, &estimator)?;
        assert!(plan_episodes_with_estimator(&history, 1, &estimator).is_err());
        assert!(snapshot.plan(1).is_err());
        Ok(())
    }
}
