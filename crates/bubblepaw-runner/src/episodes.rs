//! Pure episode planning over canonical history. Nothing here writes coverage or prepares context.
use anyhow::{Context, Result, ensure};
use bubblepaw_core::{
    Message, MessageOrigin, Role,
    config_schema::{Node, RESTART},
    config_section, is_typed_user,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::summarization::SummarizingContext;

pub const EPISODE_TARGET_TOKENS: usize = 24_000;
pub const EPISODE_MAX_TOKENS: usize = 32_000;

/// Prefer the main request's full compatible prefix for episode gathering.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeSummaryInput {
    #[default]
    Fork,
    Episode,
}

#[path = "episode_prompts.rs"]
mod prompts;
pub use prompts::{ChapterPrompt, EpisodePrompt};

config_section! {
    /// Gathering policy, independent of the serving context pressure threshold.
    /// Token counts use the runner's canonical estimator; targets are not word counts.
    #[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
    #[serde(default)]
    pub struct EpisodeConfig {
        pub summary_input: EpisodeSummaryInput => RESTART,
        /// Episode summary prompt; `v2` unless explicitly selected.
        pub episode_prompt: EpisodePrompt => RESTART,
        /// Chapter synthesis prompt; `v1` unless explicitly selected.
        pub chapter_prompt: ChapterPrompt => RESTART,
        /// Semantic chapter decisions; disabling leaves only explicit volume rollover.
        pub decider_enabled: bool => RESTART,
        pub source_target_tokens: usize => RESTART,
        pub source_max_tokens: usize => RESTART,
        pub summary_target_tokens: usize => RESTART,
        /// Accumulated open-chapter state supplied to each phase decision.
        pub decision_context_tokens: usize => RESTART,
        pub chapter_summary_max_tokens: usize => RESTART,
        pub chapter_description_tokens: usize => RESTART,
        /// Optional volume rollover; zero keeps a chapter open until a semantic boundary.
        pub chapter_rollover_tokens: usize => RESTART,
        /// Episode-only input: 0 retains complete tool results/arguments; positive values
        /// explicitly opt into a shared lossy budget. Fork source is always unchanged.
        pub tool_evidence_tokens: usize => RESTART,
    }
}
impl Default for EpisodeConfig {
    fn default() -> Self {
        Self {
            summary_input: EpisodeSummaryInput::Fork,
            episode_prompt: EpisodePrompt::V2,
            chapter_prompt: ChapterPrompt::V1,
            decider_enabled: true,
            source_target_tokens: EPISODE_TARGET_TOKENS,
            source_max_tokens: EPISODE_MAX_TOKENS,
            summary_target_tokens: 2_000,
            decision_context_tokens: 4_000,
            chapter_summary_max_tokens: 8_000,
            chapter_description_tokens: 300,
            chapter_rollover_tokens: 0,
            tool_evidence_tokens: 0,
        }
    }
}
config_section! {
    /// Gather and serving allocations are token targets, independent of agent lifetime.
    #[derive(Clone, Debug, Deserialize, Serialize)]
    #[serde(default)]
    pub struct LcmConfig {
        #[serde(flatten)]
        pub episodes: EpisodeConfig => Node::Flatten(EpisodeConfig::FIELDS),
        /// Recent-tail target for explicit manual and Classical compaction. Automatic
        /// LCM apply starts raw history immediately after all ready episode coverage.
        pub raw_tail_tokens: usize => RESTART,
        pub index_tokens: usize => RESTART,
        pub output_reserve_tokens: usize => RESTART,
        pub apply_target_tokens: Option<usize> => RESTART,
        #[serde(default)]
        pub child_compact_threshold_tokens: Option<usize> => RESTART,
    }
}
impl Default for LcmConfig {
    fn default() -> Self {
        Self {
            episodes: Default::default(),
            raw_tail_tokens: 32_000,
            index_tokens: 2_000,
            output_reserve_tokens: 16_000,
            apply_target_tokens: None,
            child_compact_threshold_tokens: Some(100_000),
        }
    }
}
impl EpisodeConfig {
    pub fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("source_target_tokens", self.source_target_tokens),
            ("source_max_tokens", self.source_max_tokens),
            ("summary_target_tokens", self.summary_target_tokens),
            ("decision_context_tokens", self.decision_context_tokens),
            (
                "chapter_summary_max_tokens",
                self.chapter_summary_max_tokens,
            ),
            (
                "chapter_description_tokens",
                self.chapter_description_tokens,
            ),
        ] {
            ensure!(value > 0, "{name} must be positive");
        }
        ensure!(
            self.source_max_tokens >= self.source_target_tokens,
            "source_max_tokens must be at least source_target_tokens"
        );
        Ok(())
    }
}

/// A child task anchors ongoing work but is never reclassified as typed user input.
pub fn is_task_anchor(message: &Message) -> bool {
    is_typed_user(message)
        || (message.role == Role::User && message.origin == Some(MessageOrigin::ChildTask))
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeKind {
    Turns,
    OpenTurnPart,
    OversizedRound,
}

/// Global indices into the full input history; `end` is exclusive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageRange {
    pub start: usize,
    pub end: usize,
    pub first_message_id: String,
    pub last_message_id: String,
}

#[derive(Clone, Debug)]
struct SourceMessage {
    id: String,
    tokens: usize,
    typed: bool,
}

#[derive(Clone, Debug)]
pub struct PlannedEpisode {
    pub message_range: MessageRange,
    pub source_tokens: usize,
    pub kind: EpisodeKind,
    /// The covered request for a continuation; absent when the episode starts with its request.
    pub origin_request_id: Option<String>,
    pub typed_user_message_ids: Vec<String>,
    source: Vec<SourceMessage>,
}

#[derive(Clone, Debug)]
pub struct EpisodePlan {
    pub episodes: Vec<PlannedEpisode>,
    pub remainder: Option<MessageRange>,
}

#[derive(Clone, Debug)]
pub struct EpisodeSplit {
    /// Absent when the selected user message starts the planned episode.
    pub before: Option<PlannedEpisode>,
    /// Re-plan full history with this range's `start` as the new coverage end.
    pub remaining: MessageRange,
}

fn message_range(source: &[SourceMessage], start: usize, end: usize) -> MessageRange {
    MessageRange {
        start,
        end,
        first_message_id: source[0].id.clone(),
        last_message_id: source[source.len() - 1].id.clone(),
    }
}

fn episode(
    source: &[SourceMessage],
    start: usize,
    kind: EpisodeKind,
    origin_request_id: Option<String>,
) -> PlannedEpisode {
    PlannedEpisode {
        message_range: message_range(source, start, start + source.len()),
        source_tokens: source.iter().map(|message| message.tokens).sum(),
        kind,
        origin_request_id,
        typed_user_message_ids: source
            .iter()
            .filter(|message| message.typed)
            .map(|message| message.id.clone())
            .collect(),
        source: source.to_vec(),
    }
}

/// Explicit compaction may close a short prefix, but never an incomplete tool round.
pub(crate) fn manual_episode(
    history: &[Message],
    coverage_end: usize,
    end: usize,
    message_tokens: impl Fn(&Message) -> usize,
) -> Option<PlannedEpisode> {
    let end = crate::context_strategies::complete_prefix_end(history, end);
    if end <= coverage_end {
        return None;
    }
    let source: Vec<_> = history[coverage_end..end]
        .iter()
        .map(|message| SourceMessage {
            id: message.id.clone(),
            tokens: message_tokens(message),
            typed: is_typed_user(message),
        })
        .collect();
    let origin = (!is_task_anchor(&history[coverage_end]))
        .then(|| {
            history[..coverage_end]
                .iter()
                .rev()
                .find(|m| is_task_anchor(m))
                .map(|m| m.id.clone())
        })
        .flatten();
    Some(episode(&source, coverage_end, EpisodeKind::Turns, origin))
}

/// Plan only messages at or after `coverage_end`, an exclusive covered-message count.
/// Full earlier history supplies pending call IDs and the request of a continuation.
/// Token and origin callbacks must be deterministic; no store or model is called.
pub fn plan_episodes(
    history: &[Message],
    coverage_end: usize,
    message_tokens: impl Fn(&Message) -> usize,
    typed_user: impl Fn(&Message) -> bool,
) -> Result<EpisodePlan> {
    plan_episodes_with_config(
        history,
        coverage_end,
        message_tokens,
        typed_user,
        &EpisodeConfig::default(),
    )
}

pub fn plan_episodes_with_config(
    history: &[Message],
    coverage_end: usize,
    message_tokens: impl Fn(&Message) -> usize,
    typed_user: impl Fn(&Message) -> bool,
    config: &EpisodeConfig,
) -> Result<EpisodePlan> {
    config.validate()?;
    ensure!(
        coverage_end <= history.len(),
        "Coverage ends beyond history"
    );
    let typed: Vec<bool> = history.iter().map(&typed_user).collect();
    let source: Vec<SourceMessage> = history
        .iter()
        .enumerate()
        .skip(coverage_end)
        .map(|(index, message)| SourceMessage {
            id: message.id.clone(),
            tokens: message_tokens(message),
            typed: typed[index],
        })
        .collect();
    let mut prefix = vec![0_usize];
    for message in &source {
        prefix.push(
            prefix
                .last()
                .unwrap()
                .checked_add(message.tokens)
                .context("Episode token count overflow")?,
        );
    }
    let mut planner = Planner {
        history,
        source: &source,
        typed: &typed,
        prefix: &prefix,
        coverage_end,
        start: coverage_end,
        round_start: coverage_end,
        episodes: Vec::new(),
        config,
    };
    let mut pending = HashSet::new();
    for (index, message) in history.iter().enumerate() {
        if index >= coverage_end && typed[index] {
            ensure!(
                pending.is_empty(),
                "Typed request interrupts an incomplete tool round"
            );
            if planner.round_start < index {
                planner.add_round(index);
            }
            if planner.tokens(planner.start, index) >= config.source_target_tokens {
                planner.close(index, EpisodeKind::Turns);
            }
        }
        for call in &message.tool_calls {
            pending.insert(call.id.as_str());
        }
        if let Some(call_id) = &message.tool_call_id {
            pending.remove(call_id.as_str());
        }
        if index >= coverage_end && pending.is_empty() && message.role != Role::User {
            planner.add_round(index + 1);
        }
    }
    let remainder = (planner.start < history.len()).then(|| {
        message_range(
            &source[planner.start - coverage_end..],
            planner.start,
            history.len(),
        )
    });
    Ok(EpisodePlan {
        episodes: planner.episodes,
        remainder,
    })
}

struct Planner<'a> {
    history: &'a [Message],
    source: &'a [SourceMessage],
    typed: &'a [bool],
    prefix: &'a [usize],
    coverage_end: usize,
    start: usize,
    round_start: usize,
    episodes: Vec<PlannedEpisode>,
    config: &'a EpisodeConfig,
}

impl Planner<'_> {
    fn tokens(&self, start: usize, end: usize) -> usize {
        self.prefix[end - self.coverage_end] - self.prefix[start - self.coverage_end]
    }

    fn close(&mut self, end: usize, kind: EpisodeKind) {
        let origin_request_id = (!(self.typed[self.start]
            || is_task_anchor(&self.history[self.start])))
        .then(|| {
            (0..self.start)
                .rev()
                .find(|&index| self.typed[index] || is_task_anchor(&self.history[index]))
                .map(|index| self.history[index].id.clone())
        })
        .flatten();
        self.episodes.push(episode(
            &self.source[self.start - self.coverage_end..end - self.coverage_end],
            self.start,
            kind,
            origin_request_id,
        ));
        self.start = end;
    }

    fn add_round(&mut self, end: usize) {
        // Only complete rounds arrive here. Prefer the preceding round boundary
        // to exceeding the maximum; an indivisible large round stays intact.
        if self.tokens(self.start, end) > self.config.source_max_tokens
            && self.round_start > self.start
        {
            self.close(self.round_start, EpisodeKind::OpenTurnPart);
        }
        if self.tokens(self.start, end) >= self.config.source_target_tokens {
            let kind = if self.tokens(self.start, end) > self.config.source_max_tokens {
                EpisodeKind::OversizedRound
            } else {
                EpisodeKind::OpenTurnPart
            };
            self.close(end, kind);
        }
        self.round_start = end;
    }
}

/// Use the runner's existing canonical-message estimator without doing I/O.
pub fn plan_episodes_with_estimator(
    history: &[Message],
    coverage_end: usize,
    estimator: &SummarizingContext,
) -> Result<EpisodePlan> {
    plan_episodes(
        history,
        coverage_end,
        |message| estimator.message_tokens(message),
        is_typed_user,
    )
}

/// Split immediately before an offered typed user message. The prefix closes at any size.
pub fn apply_split(item: &PlannedEpisode, split_message_id: &str) -> Result<EpisodeSplit> {
    let index = item
        .source
        .iter()
        .position(|message| message.id == split_message_id && message.typed)
        .context("Split must select a typed user message in the planned episode")?;
    let before = (index > 0).then(|| {
        episode(
            &item.source[..index],
            item.message_range.start,
            item.kind,
            item.origin_request_id.clone(),
        )
    });
    let remaining = message_range(
        &item.source[index..],
        item.message_range.start + index,
        item.message_range.end,
    );
    Ok(EpisodeSplit { before, remaining })
}

/// Legacy proportional summary target for callers outside the configured worker.
/// New episode summaries use `EpisodeConfig::summary_target_tokens`.
/// Returns the nearest multiple of 100 to source tokens / 16.
/// Ties round up, with a minimum target of 100 for short subject-split episodes.
pub fn turn_summary_cap(source_tokens: usize) -> usize {
    let quotient = source_tokens / 1600;
    let remainder = source_tokens % 1600;
    // Divide before rounding so even usize::MAX cannot overflow.
    (quotient + usize::from(remainder >= 800)).max(1) * 100
}

/// Deterministic projection budget; original messages and provider reasoning are untouched.
pub(crate) fn text_tokens(text: &str) -> usize {
    token_encoder().encode_ordinary(text).len()
}
pub(crate) fn token_encoder() -> &'static tiktoken_rs::CoreBPE {
    tiktoken_rs::bpe_for_tokenizer(tiktoken_rs::tokenizer::Tokenizer::O200kBase)
        .expect("Bundled O200k tokenizer")
}
pub(crate) fn token_excerpt(text: &str, budget: usize) -> String {
    let encoder = token_encoder();
    let tokens = encoder.encode_ordinary(text);
    if tokens.len() <= budget {
        return text.into();
    }
    if budget == 0 {
        return String::new();
    }
    let marker = "\n[excerpt clipped; retrieve original by message/call ID]\n";
    let marker_tokens = text_tokens(marker);
    let available = budget.saturating_sub(marker_tokens);
    let mut head = tokens[..if budget <= marker_tokens {
        budget
    } else {
        available.div_ceil(2)
    }]
        .to_vec();
    let head = loop {
        if let Ok(text) = encoder.decode(&head) {
            break text;
        }
        head.pop();
    };
    if budget <= marker_tokens {
        return head;
    }
    let mut tail = tokens[tokens.len() - available / 2..].to_vec();
    let tail = loop {
        if let Ok(text) = encoder.decode(&tail) {
            break text;
        }
        tail.remove(0);
    };
    let mut projected = format!("{head}{marker}{tail}");
    // Token joins can change segmentation. Preserve the strict configured content budget.
    while text_tokens(&projected) > budget {
        projected.pop();
    }
    projected
}
