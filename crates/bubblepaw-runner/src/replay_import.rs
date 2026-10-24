//! Shared recorded-replay import batching and canonical-prefix readiness.
use super::ProgressWriter;
use crate::episodes::EpisodeConfig;
use anyhow::{Context, Result, ensure};
use bubblepaw_core::{ContextStrategy, Message, Role, Store, is_typed_user};
use std::collections::{HashMap, HashSet};

// Canonical counts only: never use this map for projected/masked messages with reused IDs.
pub(crate) struct ReplayWorkerGate {
    episode_config: EpisodeConfig,
    pub(super) canonical_tokens: HashMap<String, usize>,
    token_prefix: Vec<usize>,
    pub(super) coverage_end: usize,
    first_lcm_check: bool,
    classical_pending: bool,
}

impl ReplayWorkerGate {
    #[cfg(test)]
    pub(super) fn new() -> Self {
        Self::with_config(EpisodeConfig::default())
    }

    pub(crate) fn with_config(episode_config: EpisodeConfig) -> Self {
        Self {
            episode_config,
            canonical_tokens: HashMap::new(),
            token_prefix: vec![0],
            coverage_end: 0,
            first_lcm_check: true,
            classical_pending: false,
        }
    }

    pub(crate) fn observe(&mut self, message: &Message, tokens: usize) -> Result<()> {
        let total = self
            .token_prefix
            .last()
            .copied()
            .unwrap()
            .checked_add(tokens)
            .context("Replay canonical token count overflow")?;
        ensure!(
            self.canonical_tokens
                .insert(message.id.clone(), tokens)
                .is_none(),
            "Replay token cache contains duplicate message ID"
        );
        self.token_prefix.push(total);
        Ok(())
    }

    pub(crate) fn needs_lcm(
        &mut self,
        history: &[Message],
        pending_calls: &HashSet<String>,
    ) -> Result<bool> {
        let last = history.last().context("Empty replay prefix")?;
        let uncovered_tokens =
            self.token_prefix[history.len()] - self.token_prefix[self.coverage_end];
        // First activation validates all prior typed requests. Thereafter every new
        // typed request reaches the planner, including malformed pending-call input.
        // Other messages cannot close an episode until a complete non-user round,
        // and no size-based episode can be emitted below the configured target.
        if !self.first_lcm_check
            && !is_typed_user(last)
            && (uncovered_tokens < self.episode_config.source_target_tokens
                || !pending_calls.is_empty()
                || last.role == Role::User)
        {
            return Ok(false);
        }
        let plan = crate::episodes::plan_episodes_with_config(
            history,
            self.coverage_end,
            |message| self.canonical_tokens[&message.id],
            is_typed_user,
            &self.episode_config,
        )?;
        self.first_lcm_check = false;
        Ok(!plan.episodes.is_empty())
    }

    pub(crate) fn refresh_lcm(
        &mut self,
        store: &Store,
        session: &str,
        history: &[Message],
    ) -> Result<()> {
        self.coverage_end = store
            .episode_coverage_token(session)?
            .map(|id| {
                history
                    .iter()
                    .position(|message| message.id == id)
                    .map(|index| index + 1)
                    .context("Replay coverage message absent from current prefix")
            })
            .transpose()?
            .unwrap_or(0);
        Ok(())
    }

    pub(super) fn refresh(
        &mut self,
        store: &Store,
        session: &str,
        history: &[Message],
        strategy: ContextStrategy,
    ) -> Result<()> {
        match strategy {
            ContextStrategy::Lcm => self.refresh_lcm(store, session, history),
            ContextStrategy::Classical => self.refresh_classical(store, session),
        }
    }

    pub(super) fn needs_classical(&self) -> bool {
        // Match drain_classical's first early return, exactly. Before its first
        // pending checkpoint, retain the real worker and existing tail estimator.
        !self.classical_pending
    }

    fn refresh_classical(&mut self, store: &Store, session: &str) -> Result<()> {
        self.classical_pending = if let Some(checkpoint) = store.context_checkpoint(session)? {
            let covered = store.message_seq(session, &checkpoint.through_message_id)?;
            let applied = store.context_view(session)?.and_then(|view| view.frontier);
            applied.is_none_or(|frontier| covered > frontier)
        } else {
            false
        };
        Ok(())
    }
}

const REPLAY_IMPORT_BATCH_MESSAGES: usize = 100;
const REPLAY_IMPORT_BATCH_BYTES: usize = 1024 * 1024;

#[derive(Default)]
pub(crate) struct ReplayImportBatch {
    pub(super) records: Vec<Message>,
    bytes: usize,
    pub(super) committed_messages: usize,
    pub(super) committed_transactions: u64,
}

impl ReplayImportBatch {
    pub(crate) fn push(&mut self, message: &Message) -> Result<()> {
        let data = serde_json::to_string(message)?;
        self.bytes = self.bytes.saturating_add(data.len());
        self.records.push(message.clone());
        Ok(())
    }

    pub(crate) fn is_full(&self) -> bool {
        self.records.len() >= REPLAY_IMPORT_BATCH_MESSAGES
            || self.bytes >= REPLAY_IMPORT_BATCH_BYTES
    }

    pub(super) fn flush_profiled(
        &mut self,
        importer: &mut Store,
        progress: &ProgressWriter,
        compactions: usize,
    ) -> Result<()> {
        if self.records.is_empty() {
            progress.counts(
                self.committed_messages,
                compactions,
                self.committed_transactions,
            );
            return Ok(());
        }
        let _phase = progress.phase(
            "canonical_commit",
            self.committed_messages + self.records.len(),
        );
        let before = importer.write_seconds();
        let result = self.flush(importer);
        progress
            .phases
            .lock()
            .unwrap()
            .canonical_store_writes_seconds += (importer.write_seconds() - before).max(0.0);
        result?;
        progress.counts(
            self.committed_messages,
            compactions,
            self.committed_transactions,
        );
        Ok(())
    }

    pub(crate) fn flush(&mut self, importer: &mut Store) -> Result<()> {
        if self.records.is_empty() {
            return Ok(());
        }
        // Never retain a transaction across a Store read/write or an await.
        importer.append_batch(&self.records)?;
        self.committed_messages += self.records.len();
        self.committed_transactions += 1;
        self.records.clear();
        self.bytes = 0;
        Ok(())
    }
}
