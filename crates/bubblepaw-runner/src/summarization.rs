//! Immutable context chapters are a disposable model view over the unchanged archive.
#[cfg(test)]
use crate::context_strategies::{ContextSummaryStrategy, LcmStrategy, SummaryPrefixInput};
use crate::context_strategies::{
    STRATEGY_RETRIEVAL_INSTRUCTION, TYPED_USER_TOKENS, complete_prefix_end,
    typed_user_block_with_budget,
};
use crate::history_snapshot::HistorySnapshot;
use crate::{episode_worker::EpisodeWorker, runtime_note::RuntimeNotes};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ContextBlock, ContextBlockKind, ContextChapter, ContextCheckpoint,
    ContextEpisode, ContextEvent, ContextEventKind, ContextPreparation, ContextStrategy,
    ContextSubjectChapter, ContextTrigger, ContextView, ContextWriter, Message, ModelRequest,
    ModelResponse, PrepareCall, PrepareMode, PreparedContext, Provider, Role, Store,
    SubjectChapterStatus, is_typed_user,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tiktoken_rs::CoreBPE;

#[path = "episode_fork.rs"]
mod episode_fork;
#[path = "summarization_context.rs"]
mod summarization_context;
#[path = "summarization_episode_prefix.rs"]
mod summarization_episode_prefix;
#[path = "summarization_facts.rs"]
mod summarization_facts;
#[path = "summarization_hook.rs"]
mod summarization_hook;
#[path = "summarization_tokens.rs"]
mod summarization_tokens;
#[path = "summarization_view.rs"]
mod summarization_view;
#[cfg(test)]
use summarization_hook::{built_after_prepare, has_inventory, prepare_main};
pub use summarization_tokens::canonical_message_tokens;
use summarization_tokens::{TokenCache, message_fingerprint};

/// Gather and serving allocations are token targets, independent of agent lifetime.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct LcmConfig {
    #[serde(flatten)]
    pub episodes: crate::episodes::EpisodeConfig,
    /// Recent-tail target for explicit manual and Classical compaction. Automatic
    /// LCM apply starts raw history immediately after all ready episode coverage.
    pub raw_tail_tokens: usize,
    /// Legacy serving allocation, retained for config compatibility. Complete
    /// closed chapter summaries are served even when they exceed this value.
    pub closed_summary_tokens: usize,
    /// Legacy serving allocation; complete open episodes are never reduced to titles.
    pub open_summary_tokens: usize,
    pub index_tokens: usize,
    /// Legacy excerpt budget; original opening/current tasks now remain exact.
    pub task_anchor_tokens: usize,
    pub output_reserve_tokens: usize,
    pub apply_target_tokens: Option<usize>,
    #[serde(default)]
    pub child_compact_threshold_tokens: Option<usize>,
}
impl Default for LcmConfig {
    fn default() -> Self {
        Self {
            episodes: Default::default(),
            raw_tail_tokens: 32_000,
            closed_summary_tokens: 16_000,
            open_summary_tokens: 16_000,
            index_tokens: 2_000,
            task_anchor_tokens: 8_000,
            output_reserve_tokens: 16_000,
            apply_target_tokens: None,
            child_compact_threshold_tokens: Some(100_000),
        }
    }
}
impl LcmConfig {
    pub fn validate(&self) -> Result<()> {
        self.episodes.validate()?;
        ensure!(
            self.raw_tail_tokens > 0,
            "context.lcm.raw_tail_tokens must be positive"
        );
        ensure!(
            self.task_anchor_tokens > 0,
            "context.lcm.task_anchor_tokens must be positive"
        );
        ensure!(
            self.apply_target_tokens != Some(0),
            "context.lcm.apply_target_tokens must be positive"
        );
        ensure!(
            self.child_compact_threshold_tokens != Some(0),
            "context.lcm.child_compact_threshold_tokens must be positive"
        );
        Ok(())
    }
    fn apply_target(&self, threshold: usize) -> usize {
        self.apply_target_tokens.unwrap_or(threshold).min(threshold)
    }
}
pub const HISTORY_RETRIEVAL_INSTRUCTION: &str = "Continue from this memory. Original messages remain available through `tools.history_search` and `tools.history_read`. Retrieve only when a needed detail is missing, ambiguous or conflicting, or original wording is required; a fact being summarized does not itself require a lookup.";
fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}
pub const SUMMARY_PROMPT: &str = include_str!("summary_prompt.md");

#[derive(Clone, Debug)]
pub struct SummaryConfig {
    pub lcm: LcmConfig,
    pub context_window: Option<usize>,
    pub threshold_cap: usize,
    pub timeout: Duration,
    pub max_request_bytes: usize,
    pub verbatim_user_tokens: usize,
}
impl Default for SummaryConfig {
    fn default() -> Self {
        Self {
            lcm: LcmConfig::default(),
            context_window: None,
            threshold_cap: 200_000,
            timeout: Duration::from_secs(180),
            max_request_bytes: 2 * 1024 * 1024,
            verbatim_user_tokens: TYPED_USER_TOKENS,
        }
    }
}
impl SummaryConfig {
    pub fn threshold(&self) -> usize {
        self.context_window
            .unwrap_or(self.threshold_cap)
            .min(self.threshold_cap)
            .saturating_sub(self.lcm.output_reserve_tokens)
    }
}

/// Local metadata is a capacity hint, not a tokenizer or an effective-window multiplier.
pub fn cached_context_window(path: &Path, model: &str) -> Option<usize> {
    let cache: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    cache["models"]
        .as_array()?
        .iter()
        .find(|entry| entry["slug"].as_str() == Some(model))?["context_window"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n > 0)
}

#[derive(Default)]
struct Meter {
    signature: String,
    pending: usize,
    pending_bytes: usize,
    rendered_input: bool,
    anchor: Option<(usize, usize)>,
}
#[derive(Clone)]
pub struct SummarizingContext {
    store: Store,
    provider: Arc<dyn Provider>,
    worker: Arc<Mutex<Weak<EpisodeWorker>>>,
    config: SummaryConfig,
    notes: Option<Arc<RuntimeNotes>>,
    tokenizer: &'static CoreBPE,
    meters: Arc<Mutex<HashMap<String, Meter>>>,
    captures: Arc<Mutex<HashMap<String, Arc<episode_fork::CapturedRequest>>>>,
    token_cache: Arc<Mutex<TokenCache>>,
    /// Per session: why the serving prefix changed and has not been reported yet.
    prefix_changes: Arc<Mutex<HashMap<String, String>>>,
    metadata: Option<(String, PathBuf)>,
    automatic: bool,
}
impl SummarizingContext {
    pub fn new(
        store: Store,
        provider: Arc<dyn Provider>,
        config: SummaryConfig,
        notes: Option<Arc<RuntimeNotes>>,
    ) -> Result<Self> {
        config.lcm.validate()?;
        ensure!(
            config
                .context_window
                .is_none_or(|window| window > config.lcm.output_reserve_tokens),
            "Model context window must exceed the configured output reserve"
        );
        ensure!(
            config.threshold() > 0,
            "Context summarization threshold must be positive"
        );
        Ok(Self {
            store,
            provider,
            worker: Arc::new(Mutex::new(Weak::new())),
            config,
            notes,
            tokenizer: crate::episodes::token_encoder(),
            meters: Arc::default(),
            captures: Arc::default(),
            token_cache: Arc::default(),
            prefix_changes: Arc::default(),
            metadata: None,
            automatic: true,
        })
    }
    /// Record that this context deliberately changed the session's serving prefix.
    fn mark_prefix_change(&self, session: &str, reason: &str) {
        self.prefix_changes
            .lock()
            .unwrap()
            .insert(session.to_owned(), reason.to_owned());
    }
    /// Explicit compaction remains available when automatic work is disabled.
    pub fn with_auto_summarize(mut self, enabled: bool) -> Self {
        self.automatic = enabled;
        self
    }
    /// Attach after constructing the worker, which itself holds the estimator.
    pub fn set_episode_worker(&self, worker: &Arc<EpisodeWorker>) {
        *self.worker.lock().unwrap() = Arc::downgrade(worker);
    }
    /// Capacity inferred for one model must not silently apply to a model override.
    pub fn with_model_metadata(mut self, model: String, path: PathBuf) -> Self {
        self.metadata = Some((model, path));
        self
    }
    fn config_for(&self, session: &str, model: &str) -> SummaryConfig {
        let mut config = self.config.clone();
        if self
            .store
            .session(session)
            .is_ok_and(|s| s.parent_session_id.is_some())
            && let Some(threshold) = config.lcm.child_compact_threshold_tokens
        {
            config.threshold_cap = config.threshold_cap.min(threshold);
        }
        if let Some((configured_model, path)) = &self.metadata
            && configured_model != model
        {
            config.context_window = cached_context_window(path, model);
        }
        config
    }
    fn tokens(&self, text: &str) -> usize {
        self.tokenizer.encode_ordinary(text).len()
    }
    fn excerpt(&self, text: &str, budget: usize) -> String {
        let mut tokens = self.tokenizer.encode_ordinary(text);
        tokens.truncate(budget);
        loop {
            if let Ok(text) = self.tokenizer.decode(&tokens) {
                return text;
            }
            if tokens.pop().is_none() {
                return String::new();
            }
        }
    }
    fn task_anchor(message: &Message) -> bool {
        is_typed_user(message)
            || (message.role == Role::User
                && message.origin == Some(bubblepaw_core::MessageOrigin::ChildTask))
    }
    fn task_anchors(&self, history: &[Message], cutoff: Option<usize>) -> Vec<Message> {
        let Some(cutoff) = cutoff else {
            return Vec::new();
        };
        let mut indices = Vec::new();
        if let Some(index) = history.iter().position(Self::task_anchor) {
            indices.push(index);
        }
        if let Some(index) = history
            .iter()
            .position(|m| m.origin == Some(bubblepaw_core::MessageOrigin::ChildTask))
            && !indices.contains(&index)
        {
            indices.push(index);
        }
        if let Some(index) = history.iter().rposition(is_typed_user)
            && !indices.contains(&index)
        {
            indices.push(index);
        }
        if let Some(index) = history.iter().rposition(Self::task_anchor)
            && !indices.contains(&index)
        {
            indices.push(index);
        }
        indices.retain(|index| *index <= cutoff);
        indices.sort_unstable();
        indices
            .into_iter()
            .map(|index| history[index].clone())
            .collect()
    }
    pub(crate) fn message_tokens(&self, message: &Message) -> usize {
        let key = message_fingerprint(message);
        if let Some(count) = self.token_cache.lock().unwrap().counts.get(&key).copied() {
            return count;
        }
        let count = self.uncached_message_tokens(message);
        let mut cache = self.token_cache.lock().unwrap();
        #[cfg(test)]
        {
            cache.computations += 1;
        }
        cache.insert(key, count);
        count
    }
    fn uncached_message_tokens(&self, message: &Message) -> usize {
        canonical_message_tokens(message)
    }
    /// Source cadence includes readable reasoning produced by native models,
    /// even when a later serving policy no longer replays that older state.
    pub(crate) fn source_message_tokens(&self, message: &Message) -> usize {
        let mut count = self.message_tokens(message);
        let Some(state) = &message.provider_state else {
            return count;
        };
        if state["adapter"] == "anthropic-messages" {
            return count.saturating_add(self.readable_thinking_tokens(&state["content"], true));
        }
        if state["adapter"] != "chat-completions"
            && !(state.get("adapter").is_none()
                && state.as_object().is_some_and(|fields| {
                    fields.len() == 1 && fields.contains_key("reasoning_content")
                }))
        {
            return count;
        }
        let add_fields = |fields: &Value| {
            let plain = ["reasoning_content", "reasoning"]
                .iter()
                .filter_map(|key| fields[*key].as_str())
                .map(|text| self.tokens(text))
                .sum::<usize>();
            let details = fields["reasoning_details"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|detail| [detail["text"].as_str(), detail["summary"].as_str()])
                .flatten()
                .map(|text| self.tokens(text))
                .sum::<usize>();
            plain.saturating_add(details)
        };
        if let Some(rounds) = state["rounds"]
            .as_array()
            .filter(|rounds| !rounds.is_empty())
        {
            for round in rounds {
                count = count.saturating_add(add_fields(&round["message_fields"]));
            }
        } else {
            count = count.saturating_add(add_fields(state.get("message_fields").unwrap_or(state)));
        }
        count
    }
    fn readable_thinking_tokens(&self, blocks: &Value, include_withheld: bool) -> usize {
        blocks
            .as_array()
            .into_iter()
            .flatten()
            .filter(|block| block["type"] == "thinking")
            .filter(|block| include_withheld || block["unsigned_cut"] != true)
            .filter_map(|block| block["thinking"].as_str())
            .map(|text| self.tokens(text))
            .sum()
    }
    pub fn estimated_tokens(&self, request: &ModelRequest) -> usize {
        self.estimated_input(request).0
    }
    fn estimated_input(&self, request: &ModelRequest) -> (usize, bool) {
        // The adapter supplies the same messages/schema it uses on the wire,
        // including its replay policy and route/model compatibility checks.
        // Unavailable projections retain the existing canonical estimate.
        let canonical = self.tokens(&request.system_prompt)
            + self.tokens(&json!(request.tools).to_string())
            + 8
            + request
                .messages
                .iter()
                .map(|m| self.message_tokens(m))
                .sum::<usize>();
        match self
            .provider
            .estimate_input_tokens(request, &|input| {
                self.tokens(&input.to_string()).saturating_add(8)
            })
            .ok()
            .flatten()
        {
            Some(estimate) => (canonical.max(estimate), true),
            None => (canonical, false),
        }
    }
    /// Replay checkpoint copy using the existing rendering, without estimator
    /// registration, worker activation, compaction or any persisted write.
    #[cfg(any(test, feature = "bench"))]
    pub(crate) fn frozen_request(
        &self,
        session: &str,
        request: ModelRequest,
    ) -> Result<ModelRequest> {
        let threshold = self.config_for_request(session, &request).threshold();
        self.serving_view(session, request, threshold)
    }
    /// Wake episode work from committed-message events without preparing or changing the view.
    /// Returns true when the wake is handled without loading serving history.
    pub fn trigger_lcm_worker(&self, worker: &Arc<EpisodeWorker>, session: &str) -> Result<bool> {
        if !self.automatic {
            return Ok(true);
        }
        if self
            .store
            .active_session_metadata(session)?
            .is_some_and(|session| session.context_strategy == ContextStrategy::Lcm)
        {
            worker.trigger(session, 0, 0);
            return Ok(true);
        }
        Ok(false)
    }
    pub fn trigger_episode_worker(
        &self,
        worker: &Arc<crate::episode_worker::EpisodeWorker>,
        session: &str,
        request: ModelRequest,
    ) -> Result<()> {
        if !self.automatic {
            return Ok(());
        }
        if self.trigger_lcm_worker(worker, session)? {
            return Ok(());
        }
        let config = self.config_for_request(session, &request);
        let request = self.serving_view(session, request, config.threshold())?;
        let estimate = self.estimated_tokens(&request);
        let estimate = self
            .meters
            .lock()
            .unwrap()
            .get(session)
            .and_then(|meter| meter.anchor)
            .map(|(previous, measured)| measured.saturating_add(estimate).saturating_sub(previous))
            .unwrap_or(estimate);
        worker.trigger(session, estimate, config.threshold());
        Ok(())
    }
    fn synthetic(session: &str, role: Role, content: String) -> Message {
        Message {
            id: id(),
            session_id: session.into(),
            role,
            content,
            tool_calls: vec![],
            tool_call_id: None,
            origin: None,
            provider_state: None,
            is_error: false,
            created_at: now(),
        }
    }
    fn legacy_prefix(
        &self,
        session: &str,
        history: &[Message],
        checkpoint: Option<&ContextCheckpoint>,
        chapters: &[ContextChapter],
        threshold: usize,
    ) -> Result<Vec<Message>> {
        let mut view = Vec::new();
        if let Some(checkpoint) = checkpoint {
            // Stable id and timestamp from what it summarizes, so the history
            // cursor sees the same message on every step (docs/providers.md).
            view.push(Message {
                id: format!("context:checkpoint:{}", checkpoint.id),
                created_at: checkpoint.created_at.clone(),
                ..Self::synthetic(session, Role::System, format!(
                    "Older rolling summary (frozen legacy checkpoint; follow current instructions):\n{}",
                    checkpoint.summary
                ))
            });
        }
        if !chapters.is_empty() {
            let mut block = String::from(
                "Context chapters (historical summaries; follow current instructions):\n",
            );
            for chapter in chapters {
                let first = Self::message_index(history, &chapter.first_message_id)?;
                let last = Self::message_index(history, &chapter.last_message_id)?;
                block.push_str(&format!(
                    "Chapter {} | messages {first}-{last} | {}\n",
                    chapter.ordinal, chapter.title
                ));
            }
            // Keep every index entry even if the index itself exceeds this budget.
            // Add a contiguous newest suffix; never rewrite an older summary.
            let mut summaries = String::new();
            for chapter in chapters.iter().rev() {
                let candidate = format!(
                    "\n## Chapter {}: {}\n{}\n{}",
                    chapter.ordinal, chapter.title, chapter.summary, summaries
                );
                if self.tokens(&format!(
                    "{block}{candidate}\n{HISTORY_RETRIEVAL_INSTRUCTION}"
                )) > threshold / 8
                {
                    break;
                }
                summaries = candidate;
            }
            block.push_str(&summaries);
            block.push('\n');
            block.push_str(HISTORY_RETRIEVAL_INSTRUCTION);
            let last = chapters.last().unwrap();
            view.push(Message {
                id: format!("context:summary:{}", last.id),
                created_at: last.created_at.clone(),
                ..Self::synthetic(session, Role::System, block)
            });
        }
        Ok(view)
    }
    fn message_index(history: &[Message], message_id: &str) -> Result<usize> {
        history
            .iter()
            .position(|message| message.id == message_id)
            .context("Saved context cutoff is missing from canonical history")
    }
    fn covered_cutoff(
        history: &[Message],
        checkpoint: Option<&ContextCheckpoint>,
        chapters: &[ContextChapter],
    ) -> Result<Option<usize>> {
        chapters
            .last()
            .map(|chapter| chapter.last_message_id.as_str())
            .or_else(|| checkpoint.map(|checkpoint| checkpoint.through_message_id.as_str()))
            .map(|message_id| Self::message_index(history, message_id))
            .transpose()
    }
    fn register_request(
        &self,
        session: &str,
        request: &ModelRequest,
        checkpoint: Option<&ContextCheckpoint>,
        chapters: &[ContextChapter],
    ) -> Result<usize> {
        let (estimated, rendered_input) = self.estimated_input(request);
        let signature = self.request_meter_signature(session, request, checkpoint, chapters)?;
        let mut meters = self.meters.lock().unwrap();
        if meters.len() >= 64 && !meters.contains_key(session) {
            meters.clear();
        }
        let meter = meters.entry(session.into()).or_default();
        if meter.signature != signature || meter.rendered_input != rendered_input {
            meter.signature = signature;
            meter.anchor = None;
        }
        meter.pending = estimated;
        meter.pending_bytes = bubblepaw_core::prepared_request_bytes(request)?;
        meter.rendered_input = rendered_input;
        Ok(meter
            .anchor
            .map(|(previous_estimate, measured)| {
                measured
                    .saturating_add(estimated)
                    .saturating_sub(previous_estimate)
                    .max(estimated)
            })
            .unwrap_or(estimated))
    }
    fn frontier_index(
        &self,
        session: &str,
        history: &[Message],
        frontier: Option<i64>,
    ) -> Result<Option<usize>> {
        frontier
            .map(|seq| {
                let message_id = self
                    .store
                    .context_view_frontier_message_id(session, Some(seq))?
                    .context("Saved view frontier is missing")?;
                if let Some(index) = history.iter().position(|message| message.id == message_id) {
                    return Ok(Some(index));
                }
                // A runtime history slice can start strictly after the saved
                // boundary. The frozen prefix already owns its older sources.
                ensure!(
                    history
                        .first()
                        .map(|message| self.store.message_seq(session, &message.id))
                        .transpose()?
                        .is_none_or(|first| first > seq),
                    "Saved view frontier is missing from the supplied history"
                );
                Ok(None)
            })
            .transpose()
            .map(Option::flatten)
    }
    fn apply_projection(message: &mut Message, masked: &HashSet<&str>, clipped: &HashSet<&str>) {
        if masked.contains(message.id.as_str()) {
            message.content = format!(
                "[tool output {}, {} chars; history_read can fetch it]",
                message.id,
                message.content.chars().count()
            );
            // Tool outputs are normally stored without adapter state; keep any replay honest.
            if let Some(state) = &mut message.provider_state
                && let Some(items) = state["items"].as_array_mut()
            {
                for item in items {
                    if item["type"] == "function_call_output" {
                        item["output"] = json!(message.content);
                    }
                }
            }
        }
        for (index, call) in message.tool_calls.iter_mut().enumerate() {
            if clipped.contains(call.id.as_str()) {
                let original = call.arguments.to_string();
                let preview: String = original.chars().take(500).collect();
                call.arguments = json!({"_history_preview":format!("{preview} [… {} chars clipped; history_read can fetch it] (tool call {})", original.chars().count().saturating_sub(500), call.id)});
                if let Some(state) = &mut message.provider_state {
                    crate::provider::replay_state::clear_argument_layout(state, index);
                }
                if let Some(state) = &mut message.provider_state
                    && let Some(items) = state["items"].as_array_mut()
                {
                    for item in items {
                        if item["type"] == "function_call" && item["call_id"] == call.id {
                            item["arguments"] = json!(call.arguments.to_string());
                        }
                    }
                }
            }
        }
    }
    fn render_saved(
        &self,
        session: &str,
        history: &[Message],
        view: &ContextView,
    ) -> Result<Vec<Message>> {
        let cutoff = self.frontier_index(session, history, view.frontier)?;
        let masked = view
            .masked_tool_output_ids
            .iter()
            .map(String::as_str)
            .collect();
        let clipped = view
            .clipped_tool_call_ids
            .iter()
            .map(String::as_str)
            .collect();
        let mut messages = view.prefix.clone();
        for original in history.iter().skip(cutoff.map_or(0, |i| i + 1)) {
            let mut message = original.clone();
            Self::apply_projection(&mut message, &masked, &clipped);
            messages.push(message);
        }
        Ok(messages)
    }
    fn active_view(
        &self,
        session: &str,
        history: &[Message],
        checkpoint: Option<&ContextCheckpoint>,
        chapters: &[ContextChapter],
        threshold: usize,
    ) -> Result<Vec<Message>> {
        if let Some(view) = self.store.context_view(session)? {
            return self.render_saved(session, history, &view);
        }
        if self.store.session(session)?.context_strategy != ContextStrategy::Lcm {
            return Ok(history.to_vec());
        }
        // Background saves do not change provider input before the first compaction.
        let mut messages = self.legacy_prefix(session, history, checkpoint, chapters, threshold)?;
        let cutoff = Self::covered_cutoff(history, checkpoint, chapters)?;
        if let Some(index) = history.iter().rposition(is_typed_user)
            && cutoff.is_some_and(|cutoff| index <= cutoff)
        {
            messages.push(history[index].clone());
        }
        messages.extend_from_slice(&history[cutoff.map_or(0, |i| i + 1)..]);
        Ok(messages)
    }
    #[cfg(test)]
    fn tail_start(&self, history: &[Message]) -> usize {
        let mut tokens = 0;
        for index in (0..history.len()).rev() {
            tokens += self.message_tokens(&history[index]);
            if tokens >= self.config.lcm.raw_tail_tokens {
                return index;
            }
        }
        0
    }
    async fn prepare_strategy(
        &self,
        session: &str,
        mut request: ModelRequest,
        snapshot: &HistorySnapshot,
        config: &SummaryConfig,
        cancellation: CancellationToken,
    ) -> Result<PreparedContext> {
        let strategy = self.store.session(session)?.context_strategy;
        let mut checkpoint = self.store.context_checkpoint(session)?;
        if strategy == ContextStrategy::Classical {
            let applied = self
                .store
                .context_view(session)?
                .and_then(|view| view.frontier);
            let advancing = checkpoint
                .as_ref()
                .map(|checkpoint| {
                    self.store
                        .message_seq(session, &checkpoint.through_message_id)
                })
                .transpose()?
                .is_some_and(|seq| applied.is_none_or(|frontier| seq > frontier));
            let ready = if advancing {
                let candidate = self.strategy_candidate(
                    session,
                    snapshot,
                    strategy,
                    checkpoint.as_ref(),
                    config.threshold(),
                )?;
                let mut preview = request.clone();
                preview.messages = self.render_cold_saved(session, snapshot, &candidate)?;
                self.estimated_tokens(&preview) < config.threshold()
                    && bubblepaw_core::prepared_request_bytes(&preview)? <= config.max_request_bytes
            } else {
                false
            };
            if !ready {
                let worker = self
                    .worker
                    .lock()
                    .unwrap()
                    .upgrade()
                    .context("Classical context requires a summary worker")?;
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => anyhow::bail!("Context summarization cancelled"),
                    result = tokio::time::timeout(config.timeout, worker.catch_up_until(session, snapshot.tail_start(self.config.lcm.raw_tail_tokens))) => {
                        result.context("Classical context summarization timed out")??;
                    }
                }
                checkpoint = self.store.context_checkpoint(session)?;
            }
            ensure!(
                checkpoint.is_some(),
                "Classical context has no complete summary span before the raw tail"
            );
        }
        ensure!(
            !cancellation.is_cancelled(),
            "Context summarization cancelled"
        );
        let mut selected = self.strategy_candidate(
            session,
            snapshot,
            strategy,
            checkpoint.as_ref(),
            config.threshold(),
        )?;
        request.messages = self.render_cold_saved(session, snapshot, &selected)?;
        let after = self.estimated_tokens(&request);
        selected.tokens = after;
        ensure!(
            after < config.threshold(),
            "Context summarization failed: retained recent input still exceeds the context budget; view was not changed"
        );
        ensure!(
            bubblepaw_core::prepared_request_bytes(&request)? <= config.max_request_bytes,
            "Context summarization failed: prepared request exceeds the byte budget; view was not changed"
        );
        ensure!(
            !cancellation.is_cancelled(),
            "Context summarization cancelled"
        );
        self.store.save_context_view(&selected)?;
        self.mark_prefix_change(session, "lcm_apply");
        self.register_request(session, &request, None, &[])?;
        Ok(PreparedContext {
            request,
            notice: Some(format!(
                "Context prepared with {strategy:?}. Original history retained; estimated active context: {after} tokens."
            )),
            usage: None,
        })
    }
    #[allow(clippy::too_many_arguments)]
    fn prepared_episode_view(
        &self,
        session: &str,
        snapshot: &HistorySnapshot,
        legacy: &[Message],
        episodes: &[ContextEpisode],
        subject_chapters: &[ContextSubjectChapter],
        legacy_cutoff: Option<usize>,
        tail_start: usize,
        config: &SummaryConfig,
        mut request: ModelRequest,
        manual: bool,
    ) -> Result<(ContextView, ModelRequest)> {
        let saved_frontier = self
            .store
            .context_view(session)?
            .and_then(|view| view.frontier);
        let saved_cutoff = saved_frontier
            .map(|seq| {
                snapshot
                    .metadata
                    .iter()
                    .enumerate()
                    .find(|(index, _)| snapshot.seq(*index) == seq)
                    .map(|(index, _)| index)
                    .context("Saved view frontier is missing from canonical history")
            })
            .transpose()?;
        let boundaries = episodes
            .iter()
            .map(|episode| snapshot.position(&episode.last_message_id))
            .collect::<Result<Vec<_>>>()?;
        // Pressure apply consumes every ready complete episode. The exact raw
        // suffix begins immediately after coverage, regardless of a tail target.
        // Manual compaction alone retains its explicitly selected recent turn.
        let initial_cutoff = boundaries
            .iter()
            .copied()
            .filter(|end| !manual || *end < tail_start)
            .max()
            .into_iter()
            .chain(saved_cutoff)
            .chain(legacy_cutoff)
            .max();
        let mut selected = self.cold_candidate_view(
            session,
            snapshot,
            legacy,
            episodes,
            subject_chapters,
            initial_cutoff,
            config.threshold(),
            tail_start,
        )?;
        request.messages = self.render_cold_saved(session, snapshot, &selected)?;
        selected.tokens = self.estimated_tokens(&request);
        request.messages = self.render_cold_saved(session, snapshot, &selected)?;
        let after = self.estimated_tokens(&request);
        selected.tokens = after;
        Ok((selected, request))
    }
    async fn prepare_context(
        &self,
        session: &str,
        mut request: ModelRequest,
        cancellation: CancellationToken,
        manual: bool,
        focus: Option<&str>,
        pressure: Option<(usize, usize)>,
    ) -> Result<PreparedContext> {
        let strategy = self.store.session(session)?.context_strategy;
        // Disabled automatic preparation historically used ordinary history.
        // Keep that request byte-for-byte until an explicit compact publishes a
        // view; merely enabling the manual API must not append a runtime note.
        if !manual && !self.automatic && self.store.context_view(session)?.is_none() {
            return Ok(PreparedContext {
                request,
                notice: None,
                usage: None,
            });
        }
        ensure!(
            !manual || strategy == ContextStrategy::Lcm,
            "Manual compaction requires an LCM session; the session strategy is fixed"
        );
        let checkpoint = if strategy == ContextStrategy::Lcm {
            self.store.context_checkpoint(session)?
        } else {
            None
        };
        let chapters = self.store.context_chapters(session)?;
        let mut config = self.config_for_request(session, &request);
        let original_threshold = config.threshold();
        if let Some((tokens, bytes)) = pressure {
            // The physical cap includes the response reservation exactly once.
            // Final cold notes/reminders are rebuilt by core, so reserve only
            // the additions missing from this host's request projection.
            config.threshold_cap = original_threshold
                .saturating_sub(tokens)
                .saturating_add(config.lcm.output_reserve_tokens);
            config.max_request_bytes = config.max_request_bytes.saturating_sub(bytes);
            ensure!(
                config.threshold() > 0,
                "Prepared request including harness reminders exceeds the context budget ({tokens} > {original_threshold} tokens)"
            );
        }
        ensure!(
            config.threshold() > 0,
            "Model context window must exceed the configured output reserve"
        );
        let history = request.messages.clone();
        let current_system_prompt = request.system_prompt.clone();
        request = self.serving_view(session, request, config.threshold())?;
        if !manual && !self.automatic {
            return Ok(PreparedContext {
                request,
                notice: None,
                usage: None,
            });
        }
        let before = self.register_request(session, &request, checkpoint.as_ref(), &chapters)?;
        let input_before = self.estimated_tokens(&request);
        let bytes_before = bubblepaw_core::prepared_request_bytes(&request)?;
        if !manual
            && pressure.is_none()
            && before <= config.threshold()
            && bubblepaw_core::prepared_request_bytes(&request)? <= config.max_request_bytes
        {
            return Ok(PreparedContext {
                request,
                notice: None,
                usage: None,
            });
        }
        // Only metadata survives this rowwise planning pass. Candidate requests
        // hydrate their raw suffix and isolated source anchors on demand.
        // A first pressure-triggered catch-up may precede the first dispatch.
        // Keep that cold candidate explicitly distinct from an observed call.
        self.capture_episode_request(session, &request, self.provider.clone(), false);
        drop(history);
        let mut snapshot = HistorySnapshot::index(&self.store, session)?;
        let (imported_frontier, imported_prefix) = self.store.imported_history_seed(session)?;
        let saved_frontier = self
            .store
            .context_view(session)?
            .and_then(|view| view.frontier);
        let scan_start = if let Some(frontier) = saved_frontier {
            let saved_end = snapshot
                .metadata
                .iter()
                .enumerate()
                .find(|(index, _)| snapshot.seq(*index) == frontier)
                .map(|(index, _)| index + 1)
                .context("Saved view frontier is missing from canonical history")?;
            let covered_end = self
                .store
                .episode_coverage_token(session)?
                .as_deref()
                .map(|id| snapshot.position(id).map(|index| index + 1))
                .transpose()?
                .unwrap_or(0);
            saved_end.min(covered_end)
        } else if strategy == ContextStrategy::Lcm {
            // Before the first frozen view, only legacy coverage is hidden;
            // background episodes have not changed the provider's raw history.
            Self::covered_cutoff(&snapshot.metadata, checkpoint.as_ref(), &chapters)?
                .or(imported_frontier.map(|seq| {
                    snapshot
                        .metadata
                        .iter()
                        .enumerate()
                        .find(|(index, _)| snapshot.seq(*index) == seq)
                        .map(|(index, _)| index)
                        .unwrap_or(0)
                }))
                .map_or(0, |index| index + 1)
        } else {
            imported_frontier
                .map(|seq| {
                    snapshot
                        .metadata
                        .iter()
                        .enumerate()
                        .find(|(index, _)| snapshot.seq(*index) == seq)
                        .map(|(index, _)| index + 1)
                        .unwrap_or(0)
                })
                .unwrap_or(0)
        };
        snapshot.scan(scan_start, self)?;
        if strategy != ContextStrategy::Lcm {
            request.system_prompt = current_system_prompt;
            return self
                .prepare_strategy(session, request, &snapshot, &config, cancellation)
                .await;
        }
        let mut tail_start = snapshot.tail_start(self.config.lcm.raw_tail_tokens);
        if manual {
            // For short chats retain the newest typed turn. A single turn has
            // no older span to compact; do not pay for a summary or freeze it.
            tail_start = tail_start.max(
                snapshot
                    .metadata
                    .iter()
                    .rposition(is_typed_user)
                    .unwrap_or(0),
            );
            tail_start = complete_prefix_end(&snapshot.metadata, tail_start);
            let applied = self
                .store
                .context_view(session)?
                .and_then(|view| view.frontier);
            if tail_start == 0
                || applied.is_some_and(|frontier| snapshot.seq(tail_start - 1) <= frontier)
            {
                return Ok(PreparedContext {
                    request,
                    notice: Some("Nothing to compact: no older complete messages before the retained recent turn. Original history retained.".into()),
                    usage: None,
                });
            }
        }
        let worker = self.worker.lock().unwrap().upgrade();
        if manual {
            let worker = worker
                .as_ref()
                .context("Manual compaction requires a summary worker")?;
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => anyhow::bail!("Context summarization cancelled"),
                result = tokio::time::timeout(config.timeout, worker.compact_until(session, tail_start, focus)) => {
                    result.context("Manual compaction timed out")??;
                }
            }
        }

        ensure!(
            !cancellation.is_cancelled(),
            "Context summarization cancelled"
        );
        request.system_prompt = current_system_prompt;
        let episodes = self.store.context_episodes(session)?;
        let subject_chapters = self.store.context_subject_chapters(session)?;
        let mut legacy = self.legacy_prefix(
            session,
            &snapshot.metadata,
            checkpoint.as_ref(),
            &chapters,
            config.threshold(),
        )?;
        if checkpoint.is_none() && chapters.is_empty() {
            legacy.extend(imported_prefix);
        }
        let legacy_cutoff =
            Self::covered_cutoff(&snapshot.metadata, checkpoint.as_ref(), &chapters)?.or(
                imported_frontier.map(|seq| {
                    snapshot
                        .metadata
                        .iter()
                        .enumerate()
                        .find(|(index, _)| snapshot.seq(*index) == seq)
                        .map(|(index, _)| index)
                        .unwrap_or(0)
                }),
            );
        let (mut selected, mut request) = self.prepared_episode_view(
            session,
            &snapshot,
            &legacy,
            &episodes,
            &subject_chapters,
            legacy_cutoff,
            tail_start,
            &config,
            request,
            manual,
        )?;
        let advances_coverage = |view: &ContextView| {
            view.frontier
                .is_some_and(|frontier| saved_frontier.is_none_or(|old| frontier > old))
        };
        let fits = |view: &ContextView, request: &ModelRequest| -> Result<bool> {
            Ok(advances_coverage(view)
                && self.estimated_tokens(request) <= config.threshold()
                && bubblepaw_core::prepared_request_bytes(request)? <= config.max_request_bytes)
        };
        if !manual
            && !fits(&selected, &request)?
            && let Some(worker) = worker
        {
            let deadline = tokio::time::Instant::now() + config.timeout;
            let complete_end = complete_prefix_end(&snapshot.metadata, snapshot.metadata.len());
            let mut force = false;
            loop {
                if fits(&selected, &request)? {
                    break;
                }
                let previous_coverage = self.store.episode_coverage_token(session)?;
                // Await only the next range needed for admission. Ordinary
                // cadence goes first; a short complete final range is an
                // explicit pressure gather, never a hidden source truncation.
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => anyhow::bail!("Context summarization cancelled"),
                    result = tokio::time::timeout_at(deadline, worker.gather_pressure_step(session, complete_end, force, previous_coverage.clone())) => {
                        result
                            .context("Context summarization failed: gathering timed out; view was not changed")?
                            .context("Context summarization failed: gathering failed; view was not changed")?;
                    }
                }
                let episodes = self.store.context_episodes(session)?;
                let subject_chapters = self.store.context_subject_chapters(session)?;
                (selected, request) = self.prepared_episode_view(
                    session,
                    &snapshot,
                    &legacy,
                    &episodes,
                    &subject_chapters,
                    legacy_cutoff,
                    tail_start,
                    &config,
                    request,
                    manual,
                )?;
                if self.store.episode_coverage_token(session)? == previous_coverage {
                    if force {
                        break;
                    }
                    force = true;
                }
            }
        }
        let after = self.estimated_tokens(&request);
        if let Some((tokens, _)) = pressure {
            ensure!(
                after <= config.threshold(),
                "Prepared request including harness reminders exceeds the context budget ({} > {original_threshold} tokens)",
                after.saturating_add(tokens)
            );
            ensure!(
                bubblepaw_core::prepared_request_bytes(&request)? <= config.max_request_bytes,
                "Prepared request including harness reminders exceeds the byte budget"
            );
            ensure!(
                advances_coverage(&selected),
                "Prepared request including harness reminders exceeds the context budget; no new complete episode coverage is ready"
            );
        }
        ensure!(
            after <= config.threshold(),
            "Context summarization failed: retained recent input still exceeds the context budget; view was not changed"
        );
        ensure!(
            bubblepaw_core::prepared_request_bytes(&request)? <= config.max_request_bytes,
            "Context summarization failed: prepared request exceeds the byte budget; view was not changed"
        );
        ensure!(
            advances_coverage(&selected),
            "Context summarization failed: no new complete episode coverage is ready; view was not changed"
        );
        ensure!(
            !cancellation.is_cancelled(),
            "Context summarization cancelled"
        );
        self.finish_view_blocks(&mut selected, &request);
        let trigger_text = if manual {
            "User asked to compact".into()
        } else if bytes_before > config.max_request_bytes {
            format!(
                "Input reached {bytes_before} bytes, cap {}",
                config.max_request_bytes
            )
        } else {
            let measurement = if before > input_before {
                "Calibrated input"
            } else {
                "Input"
            };
            format!(
                "{measurement} reached {before} tokens, cap {}",
                config.threshold()
            )
        };
        let event = self.applied_event(
            session,
            &snapshot,
            &selected,
            &legacy,
            input_before,
            after,
            trigger_text,
            manual,
        )?;
        self.store.save_context_view_with_event(&selected, &event)?;
        self.mark_prefix_change(session, "lcm_apply");
        self.register_request(session, &request, checkpoint.as_ref(), &chapters)?;
        Ok(PreparedContext {
            request,
            notice: Some(format!(
                "Context summarized. Original history retained; estimated active context: {after} tokens."
            )),
            usage: None,
        })
    }
}

#[cfg(test)]
#[path = "summarization_legacy_tests.rs"]
mod legacy_view_tests;

#[cfg(test)]
mod token_cache_tests {
    use super::*;
    use bubblepaw_core::ToolCall;

    #[test]
    fn chat_estimate_counts_replayed_reasoning_but_not_archives_or_excluded_state() -> Result<()> {
        use crate::provider::ChatCompletionsProvider;
        use bubblepaw_core::ReasoningReplay;
        let make = |policy| -> Result<SummarizingContext> {
            let provider =
                ChatCompletionsProvider::without_key("http://127.0.0.1:1")?.with_reasoning_replay(
                    std::collections::BTreeMap::from([("test".into(), policy)]),
                )?;
            SummarizingContext::new(
                Store::in_memory()?,
                Arc::new(provider),
                SummaryConfig::default(),
                None,
            )
        };
        let all = make(ReasoningReplay::All)?;
        let none = make(ReasoningReplay::None)?;
        let mut message = SummarizingContext::synthetic("session", Role::Assistant, "done".into());
        message.provider_state = Some(json!({"adapter":"chat-completions",
            "message_fields":{"reasoning_content":" thought".repeat(100_000)}}));
        let mut request = ModelRequest {
            model: "test".into(),
            messages: vec![message],
            ..Default::default()
        };
        assert!(all.estimated_tokens(&request) > 84_000);
        assert!(none.estimated_tokens(&request) < 100);
        let expected = all.estimated_tokens(&request);
        let source = all.source_message_tokens(&request.messages[0]);
        assert!(source > 84_000);
        request.messages[0].provider_state.as_mut().unwrap()["opaque_stream_fragments"] =
            json!(["archive".repeat(400_000)]);
        assert_eq!(all.estimated_tokens(&request), expected);
        assert_eq!(all.source_message_tokens(&request.messages[0]), source);
        request.messages[0].provider_state = Some(bubblepaw_core::with_provider_state_origin(
            json!({"adapter":"chat-completions","message_fields":{"reasoning_content":" thought".repeat(100_000)}}),
            "chat-completions",
            Some("other-model"),
            "native",
        )?);
        assert!(all.estimated_tokens(&request) < 100);
        Ok(())
    }

    #[test]
    fn source_cadence_counts_readable_reasoning_from_every_continuation_round() {
        let context = estimator();
        let mut message = SummarizingContext::synthetic("session", Role::Assistant, "done".into());
        let baseline = context.source_message_tokens(&message);
        message.provider_state = Some(json!({"adapter":"chat-completions","rounds":[
            {"message_fields":{"reasoning_content":" thought".repeat(16_000)}},
            {"message_fields":{"reasoning":" thought".repeat(16_000),"reasoning_details":[{"type":"reasoning.text","text":" thought".repeat(1_000)},{"type":"reasoning.encrypted","data":"opaque".repeat(100_000)}]}}
        ]}));
        assert_eq!(context.source_message_tokens(&message), baseline + 33_000);
    }

    #[test]
    fn messages_cold_estimate_and_source_cadence_include_readable_thinking_only() -> Result<()> {
        let context = SummarizingContext::new(
            Store::in_memory()?,
            Arc::new(crate::provider::MessagesProvider::api_key(
                "synthetic",
                "http://127.0.0.1:1",
            )?),
            SummaryConfig::default(),
            None,
        )?;
        let mut assistant =
            SummarizingContext::synthetic("session", Role::Assistant, "done".into());
        let canonical = context.message_tokens(&assistant);
        assistant.provider_state = Some(json!({"adapter":"anthropic-messages", "content":[
            {"type":"thinking", "thinking":" thought".repeat(32_000), "signature":"opaque".repeat(100_000)},
            {"type":"redacted_thinking", "data":"opaque".repeat(100_000)},
            {"type":"thinking", "thinking":" thought".repeat(1_000), "unsigned_cut":true},
            {"type":"text", "text":"done"}
        ]}));
        let mut request = ModelRequest {
            model: "test".into(),
            messages: vec![
                SummarizingContext::synthetic("session", Role::User, "task".into()),
                assistant.clone(),
                SummarizingContext::synthetic("session", Role::User, "next".into()),
            ],
            ..Default::default()
        };
        let original = serde_json::to_value(&request)?;
        let (estimated, rendered) = context.estimated_input(&request);
        assert!(rendered);
        assert!((32_000..33_000).contains(&estimated));
        // Source cadence counts readable output even when an unsigned cut was
        // deliberately withheld from replay, and never tokenizes its signature.
        assert_eq!(
            context.source_message_tokens(&assistant),
            canonical + 33_000
        );
        assert_eq!(serde_json::to_value(&request)?, original);
        request.messages[1].provider_state = Some(bubblepaw_core::with_provider_state_origin(
            assistant.provider_state.unwrap(),
            "anthropic-messages",
            Some("other-model"),
            "native",
        )?);
        assert!(context.estimated_tokens(&request) < 100);
        Ok(())
    }

    #[test]
    fn messages_opaque_output_calibration_retains_residual_without_double_counting_readable_thinking()
    -> Result<()> {
        for (readable, output) in [(0, 8_000), (16_000, 24_000)] {
            let store = Store::in_memory()?;
            let session = store.create_session("opaque output calibration")?.id;
            let context = SummarizingContext::new(
                store,
                Arc::new(crate::provider::MessagesProvider::api_key(
                    "synthetic",
                    "http://127.0.0.1:1",
                )?),
                SummaryConfig::default(),
                None,
            )?;
            let mut request = ModelRequest {
                model: "test".into(),
                messages: vec![SummarizingContext::synthetic(
                    &session,
                    Role::User,
                    "task".into(),
                )],
                ..Default::default()
            };
            context.register_request(&session, &request, None, &[])?;
            let response = ModelResponse {
                text: "done".into(),
                provider_state: Some(json!({"adapter":"anthropic-messages", "content":[
                    {"type":"thinking", "thinking":" thought".repeat(readable), "signature":"opaque-signature"},
                    {"type":"redacted_thinking", "data":"opaque-redaction"},
                    {"type":"text", "text":"done"}
                ]})),
                usage: Some(bubblepaw_core::Usage {
                    input_tokens: 100,
                    output_tokens: output,
                    ..Default::default()
                }),
                ..Default::default()
            };
            context.observe_response(&session, &response);
            request.messages.push(Message {
                provider_state: response.provider_state,
                ..SummarizingContext::synthetic(&session, Role::Assistant, response.text)
            });
            request.messages.push(SummarizingContext::synthetic(
                &session,
                Role::User,
                "next".into(),
            ));
            let local = context.estimated_tokens(&request);
            assert!(local < readable + 200);
            let calibrated = context.register_request(&session, &request, None, &[])?;
            assert!(
                calibrated >= output as usize,
                "opaque residual was lost: {calibrated}"
            );
            assert!(
                calibrated < output as usize + 300,
                "readable output was counted twice: {calibrated}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn physical_input_anchor_does_not_count_new_native_reasoning_twice() -> Result<()> {
        use crate::provider::ChatCompletionsProvider;
        use bubblepaw_core::{LastRequestUsage, ReasoningReplay};
        let store = Store::in_memory()?;
        let session = store.create_session("calibration")?.id;
        store.append_batch(&[SummarizingContext::synthetic(
            &session,
            Role::User,
            "task".into(),
        )])?;
        let provider =
            ChatCompletionsProvider::without_key("http://127.0.0.1:1")?.with_reasoning_replay(
                std::collections::BTreeMap::from([("test".into(), ReasoningReplay::All)]),
            )?;
        let provider = Arc::new(provider);
        let context = SummarizingContext::new(
            store.clone(),
            provider.clone(),
            SummaryConfig {
                threshold_cap: 1_500,
                lcm: LcmConfig {
                    output_reserve_tokens: 0,
                    ..Default::default()
                },
                ..Default::default()
            },
            None,
        )?;
        let request = |messages| ModelRequest {
            model: "test".into(),
            messages,
            tools: vec![bubblepaw_core::ToolDefinition {
                name: "read".into(),
                description: "read".into(),
                input_schema: json!({"type":"object"}),
                read_only: true,
                approval: Default::default(),
            }],
            ..Default::default()
        };
        prepare_main(
            &context,
            provider.clone(),
            &session,
            request(store.messages(&session)?),
        )
        .await?;
        let state = json!({"adapter":"chat-completions","message_fields":{"reasoning_content":" thought".repeat(1_000)}});
        context.observe_response(
            &session,
            &ModelResponse {
                text: "done".into(),
                provider_state: Some(state.clone()),
                last_request_usage: Some(LastRequestUsage {
                    input_tokens: Some(100),
                    output_tokens: Some(1_001),
                }),
                ..Default::default()
            },
        );
        store.append_batch(&[Message {
            provider_state: Some(state),
            ..SummarizingContext::synthetic(&session, Role::Assistant, "done".into())
        }])?;
        let (_, prepared) = prepare_main(
            &context,
            provider.clone(),
            &session,
            request(store.messages(&session)?),
        )
        .await?;
        assert!(context.estimated_tokens(&prepared.request) > 1_000);
        assert!(
            prepared.notice.is_none(),
            "the reasoning is already present in the new rendered input"
        );
        assert!(store.context_view(&session)?.is_none());
        context.observe_response(
            &session,
            &ModelResponse {
                usage: Some(bubblepaw_core::Usage {
                    input_tokens: 1_000,
                    output_tokens: 1_001,
                    ..Default::default()
                }),
                last_request_usage: Some(LastRequestUsage {
                    input_tokens: Some(600),
                    output_tokens: Some(501),
                }),
                ..Default::default()
            },
        );
        assert!(
            context
                .meters
                .lock()
                .unwrap()
                .get(&session)
                .unwrap()
                .anchor
                .is_none(),
            "final continuation input cannot calibrate the original request shape"
        );
        Ok(())
    }

    #[test]
    fn child_serving_threshold_and_output_reserve_are_independent_configuration() -> Result<()> {
        let store = Store::in_memory()?;
        let root = store.create_session("root")?;
        let child =
            store.create_session_with_parent("child", Default::default(), Some(&root.id))?;
        let mut config = SummaryConfig {
            context_window: Some(180_000),
            ..Default::default()
        };
        config.lcm.output_reserve_tokens = 20_000;
        let context = SummarizingContext::new(
            store,
            Arc::new(crate::provider::ScriptedProvider::new("unused")),
            config,
            None,
        )?;
        assert_eq!(context.config_for(&root.id, "fake").threshold(), 160_000);
        assert_eq!(context.config_for(&child.id, "fake").threshold(), 80_000);
        Ok(())
    }

    #[test]
    fn explicit_output_allowance_reserves_once_against_root_and_child_physical_caps() -> Result<()>
    {
        let store = Store::in_memory()?;
        let root = store.create_session("root")?;
        let child =
            store.create_session_with_parent("child", Default::default(), Some(&root.id))?;
        let context = SummarizingContext::new(
            store,
            Arc::new(crate::provider::ScriptedProvider::new("unused")),
            SummaryConfig {
                context_window: Some(200_000),
                ..Default::default()
            },
            None,
        )?;
        let mut request = ModelRequest::default();
        assert_eq!(
            context.config_for_request(&root.id, &request).threshold(),
            184_000
        );
        assert_eq!(
            context.config_for_request(&child.id, &request).threshold(),
            84_000
        );
        request.max_output_tokens = Some(16_000);
        assert_eq!(
            context.config_for_request(&child.id, &request).threshold(),
            84_000
        );
        request.max_output_tokens = Some(32_000);
        assert_eq!(
            context.config_for_request(&root.id, &request).threshold(),
            168_000
        );
        assert_eq!(
            context.config_for_request(&child.id, &request).threshold(),
            68_000
        );
        Ok(())
    }

    fn estimator() -> SummarizingContext {
        SummarizingContext::new(
            Store::in_memory().unwrap(),
            Arc::new(crate::provider::ScriptedProvider::new("unused")),
            SummaryConfig::default(),
            None,
        )
        .unwrap()
    }

    fn assistant() -> Message {
        Message {
            tool_calls: vec![ToolCall {
                id: "call".into(),
                name: "read".into(),
                arguments: json!({"path":"a"}),
            }],
            ..SummarizingContext::synthetic("session", Role::Assistant, "Message α".into())
        }
    }

    #[test]
    fn cold_candidates_and_classical_user_selection_match_original_bytes() -> Result<()> {
        let store = Store::in_memory()?;
        let session = store.create_session("cold candidate equivalence")?;
        let mut context = SummarizingContext::new(
            store.clone(),
            Arc::new(crate::provider::ScriptedProvider::new("unused")),
            SummaryConfig::default(),
            None,
        )?;
        let mut history = Vec::new();
        for index in 0..6 {
            let mut message = SummarizingContext::synthetic(
                &session.id,
                if index <= 2 {
                    Role::User
                } else if index == 4 {
                    Role::Tool
                } else {
                    Role::Assistant
                },
                format!("Exact source {index}: 猫\n{}", " payload".repeat(400)),
            );
            message.id = format!("source-{index}");
            message.created_at = format!("2026-10-12T00:00:0{index}Z");
            if index == 3 {
                message.tool_calls.push(ToolCall {
                    id: "call-3".into(),
                    name: "read".into(),
                    arguments: json!({"path":"a","text":"argument".repeat(500)}),
                });
                message.provider_state = Some(
                    json!({"opaque":"signed+/==","items":[{"type":"function_call","call_id":"call-3","arguments":message.tool_calls[0].arguments.to_string()}]}),
                );
            }
            if index == 4 {
                message.tool_call_id = Some("call-3".into());
            }
            history.push(message);
        }
        store.append_batch(&history)?;
        let mut snapshot = HistorySnapshot::index(&store, &session.id)?;
        snapshot.scan(0, &context)?;
        assert!(snapshot.metadata.iter().all(|message| {
            message.content.capacity() == 0
                && message.provider_state.is_none()
                && message
                    .tool_calls
                    .iter()
                    .all(|call| call.arguments.is_null())
        }));
        assert_eq!(
            snapshot.tail_start(context.config.lcm.raw_tail_tokens),
            context.tail_start(&history)
        );
        for cutoff in [None, Some(1), Some(3), Some(5)] {
            let original =
                context.candidate_view(&session.id, &history, &[], &[], &[], cutoff, 100_000, 5)?;
            let cold = context.cold_candidate_view(
                &session.id,
                &snapshot,
                &[],
                &[],
                &[],
                cutoff,
                100_000,
                5,
            )?;
            assert_eq!(
                serde_json::to_vec(&context.render_saved(&session.id, &history, &original)?)?,
                serde_json::to_vec(&context.render_cold_saved(&session.id, &snapshot, &cold)?)?
            );
            assert_eq!(original.masked_tool_output_ids, cold.masked_tool_output_ids);
            assert_eq!(original.clipped_tool_call_ids, cold.clipped_tool_call_ids);
        }
        for budget in [0, 30, 5000] {
            context.config.verbatim_user_tokens = budget;
            assert_eq!(
                context.cold_typed_user_block(&snapshot, 4)?,
                typed_user_block_with_budget(&history[..4], context.tokenizer, budget)
            );
        }
        Ok(())
    }

    #[test]
    fn repeated_rounds_and_rendered_messages_reuse_counts_without_serialization() {
        let context = estimator();
        let mut message = assistant();
        let expected = context.uncached_message_tokens(&message);
        assert_eq!(context.message_tokens(&message), expected);
        message.id = "another-rendered-id".into();
        message.session_id = "another-session".into();
        message.created_at = "later".into();
        let request = ModelRequest {
            messages: vec![message.clone()],
            system_prompt: "instructions".into(),
            ..Default::default()
        };
        for _ in 0..3 {
            assert_eq!(context.message_tokens(&message), expected);
            assert_eq!(
                context.estimated_tokens(&request),
                context.tokens(&request.system_prompt)
                    + context.tokens(&json!(request.tools).to_string())
                    + 8
                    + expected
            );
        }
        assert_eq!(context.token_cache.lock().unwrap().computations, 1);
    }

    #[test]
    fn mutations_of_content_calls_roles_and_provider_state_invalidate_cache() {
        let context = estimator();
        let mut message = assistant();
        context.message_tokens(&message);
        message.content.push_str(" changed");
        assert_eq!(
            context.message_tokens(&message),
            context.uncached_message_tokens(&message)
        );
        message.tool_calls[0].arguments = json!({"path":"a much longer path"});
        assert_eq!(
            context.message_tokens(&message),
            context.uncached_message_tokens(&message)
        );
        message.role = Role::Tool;
        message.tool_call_id = Some("call".into());
        assert_eq!(
            context.message_tokens(&message),
            context.uncached_message_tokens(&message)
        );
        message.provider_state = Some(
            json!({"adapter":"responses","items":[{"type":"reasoning","summary":[{"text":"new provider state"}],"encrypted_content":"opaque"}]}),
        );
        assert_eq!(
            context.message_tokens(&message),
            context.uncached_message_tokens(&message)
        );
        message.provider_state.as_mut().unwrap()["items"][0]["summary"][0]["text"] =
            json!("different reasoning summary");
        assert_eq!(
            context.message_tokens(&message),
            context.uncached_message_tokens(&message)
        );
        assert_eq!(context.token_cache.lock().unwrap().computations, 6);
    }

    #[test]
    fn slim_responses_estimate_matches_original_full_state_including_mixed_order() {
        let context = estimator();
        let mut message = assistant();
        let full = json!({"adapter":"responses","items":[
            {"type":"reasoning","encrypted_content":"opaque","summary":[{"type":"summary_text","text":"thinking"}]},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":message.content}]},
            {"type":"function_call","call_id":"call","name":"read","arguments":"{ \"path\" : \"a\" }"}
        ]});
        message.provider_state = Some(full.clone());
        let expected = context.message_tokens(&message);
        message.provider_state = Some(
            crate::provider::replay_state::slim(full, &message.content, &message.tool_calls)
                .unwrap(),
        );
        assert_eq!(context.message_tokens(&message), expected);
    }
}

#[cfg(test)]
mod bound_provider_tests;
