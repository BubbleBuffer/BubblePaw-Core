//! Recorded-history replay into an isolated archive; never executes recorded tools.
use crate::{
    episode_models::{
        ChapterInput, ChapterResult, ClassicalInput, ClassicalResult, EpisodeModelConfig,
        EpisodeModels, HttpEpisodeModels, SplitInput, SplitResult, SummaryInput, SummaryResult,
    },
    episode_worker::EpisodeWorker,
    episodes::EpisodeConfig,
    pricing::PriceTable,
    provider::{ProviderEndpoints, ProviderKeys, ProviderRegistry, ScriptedProvider},
    summarization::{SummarizingContext, SummaryConfig},
};
use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ContextStrategy, Message, MessageOrigin, ModelRequest, Provider,
    ReasoningEffort, Role, Store, Usage, is_typed_user,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(test)]
#[path = "replay_contract_normalization.rs"]
mod contract_normalization;
#[cfg(test)]
use contract_normalization::normalized_replay_contract;

#[path = "replay_import.rs"]
mod import;
pub(crate) use import::{ReplayImportBatch, ReplayWorkerGate};

#[path = "replay_relay.rs"]
mod relay_transport;
#[cfg(test)]
use relay_transport::relay_configured_keys;
use relay_transport::{relay, replay_roles};

const SPEND_LIMIT: f64 = 0.50;
const FLASH_MODEL: &str = "opencode/deepseek-v4.1-flash";
const HAIKU_MODEL: &str = "opencode-messages/claude-haiku-5-5";
const PRICE_BASIS: &str = "Prices come from the embedded price table (src/prices.json): Flash peak rates; Haiku whole-prompt rates up to 100k input with the 5x tier above it; cached tokens are input subsets; cache creation bills at the uncached rate where a model publishes no cache-write rate; OpenRouter uses provider-reported usage.cost";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReplayCorpus {
    pub chat_id: String,
    #[serde(default)]
    pub recorded_model: Option<String>,
    #[serde(default)]
    pub context_window: Option<usize>,
    #[serde(default)]
    pub instructions: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub message_metadata: Value,
    #[serde(default)]
    pub boundary_gold: Option<Value>,
    #[serde(default)]
    pub relates_gold: Vec<Value>,
    #[serde(default)]
    pub provenance: Value,
}

const ARCHIVE_UNKNOWN_OUTCOME: &str = "[outcome not recorded in source archive; unknown]";
const ARCHIVE_NORMALIZATION_POLICY: &str =
    "archive-unknown-outcome-v1-global-recorded-results-before-continuation";

#[derive(Clone, Default)]
struct ArchiveNormalization {
    source_messages_total: usize,
    source_corpus_sha256: String,
    replay_corpus_sha256: String,
    // One-based canonical positions; heartbeat counts need no history/SQL reads.
    marker_steps: Vec<usize>,
}
impl ArchiveNormalization {
    fn markers_replayed(&self, canonical_messages: usize) -> usize {
        self.marker_steps
            .partition_point(|step| *step <= canonical_messages)
    }
    fn metadata(&self) -> Value {
        json!({"policy":ARCHIVE_NORMALIZATION_POLICY,"origin":"archive_unknown_outcome","source_corpus_sha256":self.source_corpus_sha256,"replay_corpus_sha256":self.replay_corpus_sha256,"position_basis":"Canonical source-plus-marker steps; typed-user turn numbers and original message IDs remain unchanged","placement":"After recorded round results, before the next non-tool continuation, or at EOF; only calls with no recorded result anywhere in the source archive","is_error":false})
    }
}

/// Close only genuinely absent archive outcomes, without executing or guessing tools.
/// A whole-source result-ID scan prevents inventing a marker for a delayed result.
fn normalize_archive(corpus: &mut ReplayCorpus) -> Result<ArchiveNormalization> {
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

#[derive(Clone)]
pub struct ReplayOptions {
    /// Host limits and internal-worker retention policy for recall probes.
    pub agents: crate::config::Agents,
    /// Price table for every metered call; production passes `PriceTable::shared()`.
    pub prices: &'static PriceTable,
    pub chat: String,
    pub out: PathBuf,
    pub threshold: Option<usize>,
    pub dry_run: bool,
    pub context_strategy: ContextStrategy,
    pub probes: Option<PathBuf>,
    pub checkpoints_at_turns: Vec<usize>,
    pub probe_plan: Option<PathBuf>,
    pub plan_probes: bool,
    pub capture_turn_frontiers: bool,
    pub probe_limit: Option<usize>,
    pub max_cost_usd: Option<f64>,
}

#[derive(Serialize)]
struct Telemetry {
    max_cost_usd: f64,
    calls: Vec<Value>,
    flash_cost_usd: f64,
    jev_cost_usd: f64,
    openai_cost_usd: f64,
    haiku_cost_usd: f64,
    reserved_unknown_cost_usd: f64,
    failures: usize,
    retries: usize,
    consecutive_failures: usize,
    stopped: Option<String>,
    current_kind: String,
    current_operation: usize,
    physical_call_index: usize,
    request_slot: usize,
    http_attempt: usize,
    previous_attempt_failed: bool,
    episode_kinds: HashMap<String, String>,
}
impl Default for Telemetry {
    fn default() -> Self {
        Self {
            max_cost_usd: SPEND_LIMIT,
            calls: vec![],
            flash_cost_usd: 0.0,
            jev_cost_usd: 0.0,
            openai_cost_usd: 0.0,
            haiku_cost_usd: 0.0,
            reserved_unknown_cost_usd: 0.0,
            failures: 0,
            retries: 0,
            consecutive_failures: 0,
            stopped: None,
            current_kind: String::new(),
            current_operation: 0,
            physical_call_index: 0,
            request_slot: 0,
            http_attempt: 0,
            previous_attempt_failed: false,
            episode_kinds: HashMap::new(),
        }
    }
}

impl Telemetry {
    fn begin_operation(&mut self, kind: &str) {
        self.current_kind = kind.into();
        self.current_operation += 1;
        self.physical_call_index = 0;
        self.request_slot = 0;
        self.http_attempt = 0;
        self.previous_attempt_failed = false;
    }

    /// A successful request followed by another call in the same operation is
    /// an intentional model repetition (Jev) or structural repair (summary).
    /// Only an immediately preceding failed HTTP/transport attempt makes the
    /// next physical call a transport retry. Operation boundaries reset this.
    fn observe_attempt(&mut self, ok: bool) -> bool {
        self.physical_call_index += 1;
        let retry = self.previous_attempt_failed;
        if retry {
            self.http_attempt += 1;
            self.retries += 1;
        } else {
            self.request_slot += 1;
            self.http_attempt = 1;
        }
        self.previous_attempt_failed = !ok;
        if ok {
            self.consecutive_failures = 0;
        } else {
            self.failures += 1;
            self.consecutive_failures += 1;
        }
        retry
    }

    fn guard(&self) -> Result<()> {
        ensure!(
            self.stopped.is_none(),
            "Replay stopped by spend/failure guard"
        );
        ensure!(
            self.flash_cost_usd
                + self.jev_cost_usd
                + self.openai_cost_usd
                + self.haiku_cost_usd
                + self.reserved_unknown_cost_usd
                < self.max_cost_usd,
            "Replay spend limit reached"
        );
        Ok(())
    }
    fn reserve_next(&mut self, reservation: f64) -> bool {
        let allowed = self.guard().is_ok()
            && self.flash_cost_usd
                + self.jev_cost_usd
                + self.openai_cost_usd
                + self.haiku_cost_usd
                + self.reserved_unknown_cost_usd
                + reservation
                <= self.max_cost_usd;
        if !allowed && self.stopped.is_none() {
            self.stopped = Some(format!(
                "Spend guard: measured plus unknown spend and worst-case next response exceeds ${:.6}",
                self.max_cost_usd
            ));
        }
        allowed
    }
}

#[path = "replay_probe_model.rs"]
mod probe_transport;
use probe_transport::ReplayProbeModel;

struct ReplayModels {
    inner: Option<Arc<dyn EpisodeModels>>,
    telemetry: Arc<Mutex<Telemetry>>,
    store: Store,
    estimator: Arc<SummarizingContext>,
    episode_config: EpisodeConfig,
    roles: crate::model_roles::ModelRoles,
    prices: &'static PriceTable,
}
impl ReplayModels {
    fn begin(&self, kind: &str) -> Result<()> {
        let mut meter = self.telemetry.lock().unwrap();
        meter.guard()?;
        meter.begin_operation(kind);
        Ok(())
    }
    fn fake_call(&self, usage: &Usage) {
        let mut meter = self.telemetry.lock().unwrap();
        let kind = meter.current_kind.clone();
        let operation = meter.current_operation;
        let projected = if kind == "decision" {
            // OpenRouter tariffs come from live reported usage.cost only, so an
            // unpriced decider projects no scripted spend.
            let model = self.roles.decider.models.first().map_or("", String::as_str);
            self.prices.usage_cost(model, usage, None).unwrap_or(0.0)
        } else {
            let role = if kind == "chapter" {
                &self.roles.chapter
            } else {
                &self.roles.summary
            };
            self.prices
                .usage_cost(&role.models[0], usage, None)
                .expect("validated replay price")
        };
        meter.calls.push(json!({"kind":kind,"operation":operation,"physical_call_index":1,"request_slot":1,"attempt":1,"transport_retry":false,"outcome":"scripted","input_tokens":usage.input_tokens,"output_tokens":usage.output_tokens,"reasoning_tokens":0,"cost_usd":0.0,"projected_paid_cost_usd":projected,"latency_ms":0}));
    }
    fn record_kind(&self, session: &str) -> Result<()> {
        let mut snapshot = crate::history_snapshot::HistorySnapshot::index(&self.store, session)?;
        let coverage = self.store.episode_coverage_token(session)?;
        let end = coverage
            .as_ref()
            .map(|id| snapshot.position(id).map(|index| index + 1))
            .transpose()?
            .unwrap_or(0);
        if self
            .store
            .context_episodes(session)?
            .last()
            .is_some_and(|episode| Some(&episode.last_message_id) == coverage.as_ref())
        {
            snapshot.seed_complete_prefix(end);
        }
        snapshot.scan(end, &self.estimator)?;
        if let Some(planned) = snapshot
            .plan_with_config(end, &self.episode_config)?
            .episodes
            .first()
        {
            let kind = serde_json::to_value(planned.kind)?
                .as_str()
                .unwrap()
                .to_owned();
            self.telemetry
                .lock()
                .unwrap()
                .episode_kinds
                .insert(planned.message_range.first_message_id.clone(), kind);
        }
        Ok(())
    }
}
/// Scripted episode memory written as a summary would be: one prose line per
/// recorded message with its source ID and a short excerpt, never the
/// rendered transcript itself (that is invalid memory, as for a live model).
fn scripted_episode_memory(episode_text: &str, cap: usize) -> String {
    static HEADER: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let header = HEADER.get_or_init(|| {
        regex::Regex::new(r"(?m)^\[(\S+) (User|Assistant|Tool|System)\]$").unwrap()
    });
    let starts: Vec<_> = header.captures_iter(episode_text).collect();
    let mut lines = Vec::new();
    for (index, captures) in starts.iter().enumerate() {
        let whole = captures.get(0).unwrap();
        let end = starts
            .get(index + 1)
            .map_or(episode_text.len(), |next| next.get(0).unwrap().start());
        let body = &episode_text[whole.end()..end];
        let excerpt = body
            .lines()
            .map(str::trim)
            .find(|line| {
                !line.is_empty()
                    && !line.starts_with("[tool output ")
                    && !line.starts_with("Tool call ")
                    && !line.starts_with("[source ")
            })
            .and_then(|line| {
                // Keep the excerpt verbatim (the scripted decision reads the
                // `[new subject]` marker); defuse brackets only when needed.
                let words = line
                    .split_whitespace()
                    .take(24)
                    .collect::<Vec<_>>()
                    .join(" ");
                [words.clone(), words.replace(['[', ']', '`'], "'")]
                    .into_iter()
                    .find(|line| {
                        crate::episode_models::summary_output_reason("summary", line).is_none()
                    })
            })
            .unwrap_or_else(|| "(no plain text)".into());
        // The scripted decision reads the marker wherever the message has it.
        let excerpt = if body.contains("[new subject]") && !excerpt.contains("[new subject]") {
            format!("[new subject] {excerpt}")
        } else {
            excerpt
        };
        let role = match &captures[2] {
            "User" => "The user wrote",
            "Assistant" => "The agent wrote",
            "Tool" => "A tool returned",
            _ => "The host noted",
        };
        lines.push(format!("- {role} (source {}): {excerpt}", &captures[1]));
    }
    let memory = format!(
        "# Scripted replay episode\nThe recorded chat covered {} messages.\n{}\n\nOpen: Continue the recorded chat.",
        starts.len(),
        lines.join("\n")
    );
    let memory = crate::episodes::token_excerpt(&memory, cap);
    if crate::episode_models::summary_output_reason("summary", &memory).is_none() {
        memory
    } else {
        format!(
            "# Scripted replay episode\nThe recorded chat covered {} messages.\n\nOpen: Continue the recorded chat.",
            starts.len()
        )
    }
}
#[path = "replay_models.rs"]
mod replay_models;

pub async fn run(options: ReplayOptions) -> Result<Value> {
    run_with_config(
        options,
        EpisodeModelConfig::default(),
        ProviderEndpoints::default(),
    )
    .await
}

pub async fn run_with_config(
    options: ReplayOptions,
    config: EpisodeModelConfig,
    endpoints: ProviderEndpoints,
) -> Result<Value> {
    run_with_context_config(options, config, endpoints, default_replay_summary_config()).await
}

/// Host configuration, with the ordinary replay defaults retained by the wrappers.
pub async fn run_with_context_config(
    options: ReplayOptions,
    config: EpisodeModelConfig,
    endpoints: ProviderEndpoints,
    summary_config: SummaryConfig,
) -> Result<Value> {
    replay_roles(
        &config,
        options.prices,
        summary_config.lcm.episodes.decider_enabled,
    )?;
    let corpus_path = Path::new(&options.chat);
    let canonical = if corpus_path.is_file() && corpus_path.extension().is_some_and(|e| e == "json")
    {
        let value: Value = serde_json::from_slice(&std::fs::read(corpus_path)?)?;
        value.get("messages").is_some().then_some(value)
    } else {
        None
    };
    let Some(canonical) = canonical else {
        bail!(
            "Replay accepts only canonical corpus JSON (an object with \"messages\"); \
             recorded chats in other formats are not supported"
        );
    };
    std::fs::create_dir_all(&options.out)?;
    ensure!(
        !options.out.join("store").exists() && !options.out.join("report.json").exists(),
        "Replay output already contains a run; choose a fresh directory"
    );
    let corpus = serde_json::from_value(canonical)?;
    let source_corpus_raw_sha256 = format!("{:x}", Sha256::digest(std::fs::read(corpus_path)?));
    let out = options.out.clone();
    if options.plan_probes {
        let mut report =
            build_probe_plan(corpus, options, config, endpoints, summary_config).await?;
        report["source_corpus_raw_sha256"] = json!(source_corpus_raw_sha256);
        std::fs::write(out.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
        return Ok(report);
    }
    let mut report =
        run_corpus_with_context_config(corpus, options, config, endpoints, summary_config).await?;
    report["source_corpus_raw_sha256"] = json!(source_corpus_raw_sha256);
    report["final_artifact_storage"] = physical_storage(&out.join("store"))?;
    std::fs::write(out.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
    std::fs::write(out.join("report.md"), markdown(&report)?)?;
    Ok(report)
}

/// Filesystem-only observation avoids opening SQLite or checkpointing a live replay.
fn physical_storage(data_dir: &Path) -> Result<Value> {
    let bytes = |path: &Path| std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let serving_bytes = bytes(&data_dir.join("serving.db"));
    let catalog_bytes = bytes(&data_dir.join("catalog.db"));
    let mut chat_bytes = 0;
    let mut wal_bytes =
        bytes(&data_dir.join("serving.db-wal")) + bytes(&data_dir.join("catalog.db-wal"));
    let chats = data_dir.join("chats");
    if chats.exists() {
        for entry in std::fs::read_dir(chats)? {
            let path = entry?.path();
            if path.extension().is_some_and(|extension| extension == "db") {
                chat_bytes += bytes(&path);
                wal_bytes += bytes(&PathBuf::from(format!("{}-wal", path.display())));
            }
        }
    }
    Ok(
        json!({"database_bytes": chat_bytes + serving_bytes + catalog_bytes,
        "chat_bytes": chat_bytes, "serving_bytes": serving_bytes,
        "catalog_bytes": catalog_bytes, "wal_bytes": wal_bytes,
        "basis": "Physical layout artifacts; filesystem stats only, no database reopening"}),
    )
}

pub async fn run_corpus(corpus: ReplayCorpus, options: ReplayOptions) -> Result<Value> {
    run_corpus_with_config(
        corpus,
        options,
        EpisodeModelConfig::default(),
        ProviderEndpoints::default(),
    )
    .await
}

pub async fn run_corpus_with_config(
    corpus: ReplayCorpus,
    options: ReplayOptions,
    config: EpisodeModelConfig,
    endpoints: ProviderEndpoints,
) -> Result<Value> {
    run_corpus_with_context_config(
        corpus,
        options,
        config,
        endpoints,
        default_replay_summary_config(),
    )
    .await
}

fn default_replay_summary_config() -> SummaryConfig {
    // Archive replay has no live main request to fork. This constructor selects
    // source-only input explicitly; callers supplying context config own that policy.
    let mut config = SummaryConfig::default();
    config.lcm.episodes.summary_input = crate::episodes::EpisodeSummaryInput::Episode;
    config
}

fn replay_summary_config(
    corpus: &ReplayCorpus,
    options: &ReplayOptions,
    summary: &SummaryConfig,
) -> SummaryConfig {
    SummaryConfig {
        context_window: if options.threshold.is_some() {
            None
        } else {
            summary.context_window.or(corpus.context_window)
        },
        // Replay's explicit --threshold remains an input threshold. It is not
        // the live runner's physical context cap, which includes output reserve.
        threshold_cap: options
            .threshold
            .map_or(summary.threshold_cap, |threshold| {
                threshold.saturating_add(summary.lcm.output_reserve_tokens)
            }),
        ..summary.clone()
    }
}

fn resolved_threshold(
    corpus: &ReplayCorpus,
    options: &ReplayOptions,
    summary: &SummaryConfig,
) -> usize {
    replay_summary_config(corpus, options, summary).threshold()
}
fn turn_end_steps(history: &[Message]) -> Vec<usize> {
    let starts: Vec<_> = history
        .iter()
        .enumerate()
        .filter(|(_, message)| is_typed_user(message))
        .map(|(index, _)| index)
        .collect();
    starts
        .iter()
        .skip(1)
        .copied()
        .chain((!starts.is_empty()).then_some(history.len()))
        .collect()
}
fn model_signature(
    config: &EpisodeModelConfig,
    endpoints: &ProviderEndpoints,
    summary: &SummaryConfig,
    threshold: usize,
) -> Value {
    let role = |role: &crate::model_roles::ModelRole| json!({"models":role.models,"repeats":role.repeats,"threshold":role.threshold,"reasoning_effort":role.reasoning_effort});
    json!({"probe":crate::probes::metadata_for(&config.roles.summary.models[0], config.roles.summary.reasoning_effort),"summary":role(&config.roles.summary),"chapter":role(&config.roles.chapter),"decider":role(&config.roles.decider),"decider_enabled":summary.lcm.episodes.decider_enabled,"threshold":threshold,"verbatim_user_tokens":summary.verbatim_user_tokens,"provider_reasoning":format!("{:?}",endpoints)})
}
fn validate_plan(
    plan: &crate::probes::ProbePlan,
    corpus: &ReplayCorpus,
    threshold: usize,
    signature: &Value,
) -> Result<()> {
    crate::probes::validate(&plan.probes, &corpus.messages)?;
    ensure!(
        plan.corpus_sha256 == crate::probes::fingerprint(&(ARCHIVE_NORMALIZATION_POLICY, corpus))?,
        "Probe plan corpus identity differs"
    );
    ensure!(
        plan.threshold == threshold,
        "Probe plan threshold differs; use its pinned context.compact_threshold_tokens"
    );
    ensure!(
        &plan.model_signature == signature,
        "Probe plan model/role or context configuration differs"
    );
    let expected = crate::probes::common_checkpoints(
        &plan.probes,
        &corpus.messages,
        &plan.checkpoints_at_turns,
        &plan.all_turn_frontiers,
        plan.probe_limit_per_checkpoint,
    )?;
    ensure!(
        crate::probes::fingerprint(&expected)? == crate::probes::fingerprint(&plan.checkpoints)?,
        "Probe plan common eligibility/list differs from pinned arm frontiers"
    );
    let steps = turn_end_steps(&corpus.messages);
    ensure!(
        plan.checkpoints
            .iter()
            .all(|checkpoint| steps.get(checkpoint.turn - 1) == Some(&checkpoint.step)),
        "Probe plan end-of-turn positions differ from corpus"
    );
    Ok(())
}
fn turn_snapshot(
    store: &Store,
    session: &str,
    history: &[Message],
    turn: usize,
    compaction_count: usize,
    tokens: usize,
    pending: bool,
) -> Result<Value> {
    let view = store.context_view(session)?;
    let frontier = view.as_ref().and_then(|view| view.frontier);
    Ok(
        json!({"turn":turn,"step":history.len(),"end_message_id":history.last().map(|m|&m.id),"frontier_message_id":store.context_view_frontier_message_id(session,frontier)?,"compaction_count":compaction_count,"view_tokens":tokens,"status":if pending{"incomplete_tool_round"}else{"complete"}}),
    )
}

fn checkpoint_mismatch(
    planned: &crate::probes::PlanCheckpoint,
    arm: &str,
    probes: &[crate::probes::Probe],
    history: &[Message],
    frontier_id: Option<&str>,
) -> bool {
    planned.arm_frontiers[arm]["frontier_message_id"].as_str() != frontier_id
        || planned.common_probe_ids.iter().any(|id| {
            !crate::probes::eligible(
                probes.iter().find(|probe| &probe.id == id).unwrap(),
                planned.checkpoint,
                planned.turn,
                history,
                frontier_id,
            )
        })
}

/// An explicit free planning run, reused by every paid arm and derived subsets.
pub async fn build_probe_plan(
    mut corpus: ReplayCorpus,
    options: ReplayOptions,
    config: EpisodeModelConfig,
    endpoints: ProviderEndpoints,
    summary: SummaryConfig,
) -> Result<Value> {
    use crate::probes::{
        ProbePlan, common_checkpoints, fingerprint, load_plan, save_plan, strategy_signature,
    };
    let archive_normalization = normalize_archive(&mut corpus)?;
    replay_roles(
        &config,
        options.prices,
        summary.lcm.episodes.decider_enabled,
    )?;
    ensure!(
        !options.out.join("probe-plan.json").exists() && !options.out.join("report.json").exists(),
        "Planning output already contains a run"
    );
    std::fs::create_dir_all(&options.out)?;
    let threshold = resolved_threshold(&corpus, &options, &summary);
    let signature = model_signature(&config, &endpoints, &summary, threshold);
    let mut reports = BTreeMap::new();
    let (probes, all_turn_frontiers, source_plan_sha256, original_positions) =
        if let Some(path) = &options.probe_plan {
            ensure!(
                options.probes.is_none(),
                "Derived plan uses the embedded pinned probes"
            );
            let (plan, digest) = load_plan(path)?;
            validate_plan(&plan, &corpus, threshold, &signature)?;
            (
                plan.probes,
                plan.all_turn_frontiers,
                Some(digest),
                plan.checkpoints_at_turns,
            )
        } else {
            let probes = crate::probes::load(
                options
                    .probes
                    .as_ref()
                    .context("--plan-probes requires --probes or --probe-plan")?,
                &corpus.messages,
            )?;
            let mut frontiers = BTreeMap::new();
            for strategy in [ContextStrategy::Lcm, ContextStrategy::Classical] {
                let mut arm = options.clone();
                let name = serde_json::to_value(strategy)?.as_str().unwrap().to_owned();
                arm.out = options.out.join(&name);
                arm.context_strategy = strategy;
                arm.dry_run = true;
                arm.probes = None;
                arm.probe_plan = None;
                arm.plan_probes = false;
                arm.checkpoints_at_turns = vec![];
                arm.probe_limit = None;
                arm.capture_turn_frontiers = true;
                let report = run_corpus_with_context_config(
                    corpus.clone(),
                    arm,
                    config.clone(),
                    endpoints.clone(),
                    summary.clone(),
                )
                .await?;
                frontiers.insert(
                    name.clone(),
                    report["turn_frontiers"]
                        .as_array()
                        .context("Planner lacks turn frontiers")?
                        .clone(),
                );
                reports.insert(name, report);
            }
            // Keep both archived arm diagnostics even when one cannot replay
            // the source, while still refusing to publish an incomplete plan.
            for (name, report) in &reports {
                ensure!(
                    report["status"] == "complete",
                    "Scripted planning arm {name} failed"
                );
            }
            (probes, frontiers, None, vec![])
        };
    let ask_positions = crate::probes::ask_positions(&probes);
    let positions = if !options.checkpoints_at_turns.is_empty() {
        options.checkpoints_at_turns.clone()
    } else if !original_positions.is_empty() {
        original_positions
    } else if !ask_positions.is_empty() {
        ask_positions
    } else {
        let n = turn_end_steps(&corpus.messages).len();
        ensure!(n > 0, "Corpus has no typed-user turns");
        let mut positions = vec![12.min(n), 22.min(n), n];
        positions.sort_unstable();
        positions.dedup();
        positions
    };
    ensure!(
        positions[0] > 0 && positions.windows(2).all(|pair| pair[0] < pair[1]),
        "Plan positions must be positive and increasing"
    );
    ensure!(
        options.probe_limit.is_none_or(|limit| limit > 0),
        "Probe limit must be positive"
    );
    let checkpoints = common_checkpoints(
        &probes,
        &corpus.messages,
        &positions,
        &all_turn_frontiers,
        options.probe_limit,
    )?;
    let derived = source_plan_sha256.is_some();
    let plan = ProbePlan {
        schema_version: 1,
        corpus_sha256: archive_normalization.replay_corpus_sha256.clone(),
        probes_sha256: fingerprint(&probes)?,
        threshold,
        checkpoints_at_turns: positions,
        model_signature: signature,
        strategy_signature: strategy_signature(),
        probe_limit_per_checkpoint: options.probe_limit,
        probes,
        checkpoints,
        all_turn_frontiers,
        source_plan_sha256,
    };
    let digest = save_plan(&options.out.join("probe-plan.json"), &plan)?;
    let mut report = json!({"mode":"scripted_planning","status":"complete","paid_calls":0,"planning_model_calls":reports.values().map(|report|report["calls"].as_array().map_or(0,Vec::len)).sum::<usize>(),"probe_plan":"probe-plan.json","plan_sha256":digest,"plan":plan,"arms":reports,"checkpoint_semantics":"End of complete typed-user turn K, before the next typed user message; ask_at_turn fixes an individual probe position; earliest_checkpoint is the configured position-list ordinal","derived_without_replay":derived});
    report["corpus_sha256"] = json!(archive_normalization.source_corpus_sha256);
    report["source_messages_total"] = json!(archive_normalization.source_messages_total);
    report["archive_unknown_outcome_markers_total"] =
        json!(archive_normalization.marker_steps.len());
    report["archive_normalization"] = archive_normalization.metadata();
    std::fs::write(
        options.out.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    let mut text = String::from(
        "# Common context probe plan\n\nCheckpoints are ends of complete typed-user turns. One pinned probe list is shared by both arms. Positioned probes are asked only at ask_at_turn; earliest_checkpoint refers to the position-list ordinal. Planning is scripted and makes no paid calls.\n\n| Checkpoint | Turn | End step | Common probes | LCM compactions | Classical compactions |\n|---:|---:|---:|---:|---:|---:|\n",
    );
    for checkpoint in &plan.checkpoints {
        text.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            checkpoint.checkpoint,
            checkpoint.turn,
            checkpoint.step,
            checkpoint.common_probe_ids.len(),
            checkpoint.arm_frontiers["lcm"]["compaction_count"],
            checkpoint.arm_frontiers["classical"]["compaction_count"],
        ));
    }
    std::fs::write(options.out.join("report.md"), text)?;
    Ok(report)
}

pub async fn run_corpus_with_context_config(
    corpus: ReplayCorpus,
    options: ReplayOptions,
    config: EpisodeModelConfig,
    endpoints: ProviderEndpoints,
    summary_config: SummaryConfig,
) -> Result<Value> {
    run_corpus_inner(
        corpus,
        options,
        config,
        endpoints,
        summary_config,
        ReplayExecution::Optimized,
        #[cfg(test)]
        None,
    )
    .await
}

#[derive(Clone, Copy)]
enum ReplayExecution {
    Optimized,
    #[cfg(test)]
    PerMessage,
}
impl ReplayExecution {
    fn optimized(self) -> bool {
        matches!(self, Self::Optimized)
    }
}

async fn run_corpus_inner(
    mut corpus: ReplayCorpus,
    mut options: ReplayOptions,
    mut config: EpisodeModelConfig,
    mut endpoints: ProviderEndpoints,
    summary_config: SummaryConfig,
    execution: ReplayExecution,
    #[cfg(test)] prepared_trace: Option<&Mutex<Vec<(usize, ModelRequest)>>>,
) -> Result<Value> {
    ensure!(
        !options.plan_probes,
        "Use build_probe_plan or the replay planning command"
    );
    let archive_normalization = normalize_archive(&mut corpus)?;
    let corpus_sha256 = archive_normalization.source_corpus_sha256.clone();
    let threshold = resolved_threshold(&corpus, &options, &summary_config);
    let model_signature = model_signature(&config, &endpoints, &summary_config, threshold);
    let pinned_plan = if let Some(path) = &options.probe_plan {
        ensure!(
            options.probes.is_none()
                && options.checkpoints_at_turns.is_empty()
                && options.probe_limit.is_none(),
            "--probe-plan supplies probes, positions and limit; reject per-arm overrides"
        );
        let (plan, digest) = crate::probes::load_plan(path)?;
        validate_plan(&plan, &corpus, threshold, &model_signature)?;
        options.checkpoints_at_turns = plan.checkpoints_at_turns.clone();
        // Pinned comparisons archive checkpoint hashes and compact metrics, not
        // hundreds of redundant human-readable serving-view dumps.
        options.capture_turn_frontiers = true;
        Some((plan, digest))
    } else {
        None
    };
    replay_roles(
        &config,
        options.prices,
        summary_config.lcm.episodes.decider_enabled,
    )?;
    let started = Instant::now();
    ensure!(!corpus.messages.is_empty(), "Recorded chat is empty");
    ensure!(
        options
            .max_cost_usd
            .is_none_or(|cost| cost.is_finite() && cost > 0.0),
        "--max-cost-usd must be finite and positive"
    );
    ensure!(
        options.checkpoints_at_turns.iter().all(|turn| *turn > 0)
            && options
                .checkpoints_at_turns
                .windows(2)
                .all(|pair| pair[0] < pair[1]),
        "--checkpoints-at-turns must contain positive, increasing turn positions"
    );
    ensure!(
        options.probes.is_none(),
        "--probes is accepted only by --plan-probes; all answering runs, scripted or paid, require --probe-plan"
    );
    ensure!(
        options.probe_limit.is_none_or(|limit| limit > 0),
        "--probe-limit must be positive"
    );
    let probes = if let Some((plan, _)) = &pinned_plan {
        plan.probes.clone()
    } else {
        options
            .probes
            .as_ref()
            .map(|path| crate::probes::load(path, &corpus.messages))
            .transpose()?
            .unwrap_or_default()
    };
    let turn_steps = turn_end_steps(&corpus.messages);
    ensure!(
        options
            .checkpoints_at_turns
            .iter()
            .all(|turn| *turn <= turn_steps.len()),
        "Requested checkpoint exceeds typed-user turn count"
    );
    let unique: HashSet<_> = corpus.messages.iter().map(|m| &m.id).collect();
    ensure!(
        unique.len() == corpus.messages.len(),
        "Recorded chat contains duplicate message IDs"
    );
    std::fs::create_dir_all(&options.out)?;
    let data_dir = options.out.join("store");
    ensure!(
        !data_dir.exists(),
        "Replay database already exists; no overwrite or resume"
    );
    let store = Store::open_data_dir(&data_dir)?;
    let session = store.create_session_with_strategy(
        format!("Replay {}", corpus.chat_id),
        options.context_strategy,
    )?;
    let summary_config = replay_summary_config(&corpus, &options, &summary_config);
    let threshold = summary_config.threshold();
    let episode_policy = summary_config.lcm.episodes.clone();
    let fallback = Arc::new(ScriptedProvider::new("Replay fallback must not be called"));
    let estimator = Arc::new(SummarizingContext::new(
        store.clone(),
        fallback.clone(),
        summary_config,
        None,
    )?);
    let telemetry = Arc::new(Mutex::new(Telemetry {
        max_cost_usd: options.max_cost_usd.unwrap_or(SPEND_LIMIT),
        ..Telemetry::default()
    }));
    let mut relay_task = None;
    let mut probe_registry = None;
    // Scripted runs also reject unpriced configured roles before making artifacts.
    replay_roles(&config, options.prices, episode_policy.decider_enabled)?;
    let inner: Option<Arc<dyn EpisodeModels>> = if options.dry_run {
        None
    } else {
        let (task, keys) = relay(
            &mut config,
            &mut endpoints,
            telemetry.clone(),
            options.out.join("calls.jsonl"),
            options.prices,
            episode_policy.decider_enabled,
        )
        .await?;
        relay_task = Some(task);
        let mut registry = ProviderRegistry::new("replay", fallback);
        registry.set_usage_store(store.clone());
        endpoints.register_with_keys(&mut registry, keys)?;
        let registry = Arc::new(registry);
        probe_registry = Some(registry.clone());
        Some(Arc::new(
            HttpEpisodeModels::from_registry(config.clone(), registry, None)?
                .with_episode_config(episode_policy.clone())?,
        ))
    };
    let probe_model = Arc::new(ReplayProbeModel {
        context_route: Default::default(),
        telemetry: telemetry.clone(),
        prices: options.prices,
        url: if options.dry_run {
            None
        } else {
            endpoints
                .opencode_base_url
                .as_ref()
                .map(|url| format!("{}/chat/completions", url.trim_end_matches('/')))
        },
        client: reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        model: config
            .roles
            .summary
            .models
            .iter()
            .find(|model| {
                probe_registry
                    .as_ref()
                    .is_none_or(|registry| registry.llm_available(model))
            })
            .context("No replay probe summary model is available")?
            .clone(),
        effort: config.roles.summary.reasoning_effort,
        registry: probe_registry,
    });
    let models = Arc::new(ReplayModels {
        inner,
        telemetry: telemetry.clone(),
        store: store.clone(),
        estimator: estimator.clone(),
        episode_config: episode_policy.clone(),
        roles: config.roles.clone(),
        prices: options.prices,
    });
    let worker = EpisodeWorker::new_with_config(
        store.clone(),
        models,
        estimator.clone(),
        episode_policy.clone(),
    )?;
    estimator.set_episode_worker(&worker);
    // Every compaction-step request is built through this state, as the live root path does.
    let context_state = bubblepaw_core::ContextState::default();
    // Imports use the record's encoding (including blobs and message IDs),
    // without creating or replaying effects. Counts retain a separate import subtotal.
    let archive_setup_started = Instant::now();
    let mut importer = store.clone();
    let archive_setup_seconds = archive_setup_started.elapsed().as_secs_f64();
    let mut history = Vec::new();
    let mut activated = false;
    let mut pending_calls = HashSet::new();
    let mut imports = ReplayImportBatch::default();
    let mut worker_gate = ReplayWorkerGate::with_config(episode_policy.clone());
    let mut compactions = Vec::new();
    let mut probe_rows = Vec::new();
    let mut turn_frontiers = Vec::new();
    let mut plan_mismatches = Vec::new();
    let mut view_tokens_per_turn = Vec::new();
    let mut failures = Vec::new();
    let mut stopped = None;
    let mut active_tokens = estimator.estimated_tokens(&ModelRequest {
        output_schema: None,
        model: String::new(),
        reasoning_effort: None,
        system_prompt: corpus.instructions.clone(),
        messages: vec![],
        tools: vec![],
        disabled_tools: vec![],
        max_output_tokens: None,
        session_id: String::new(),
        ..Default::default()
    });
    for message in &mut corpus.messages {
        message.session_id = session.id.clone();
        if message.role == Role::User && message.origin.is_none() {
            message.origin = Some(if is_typed_user(message) {
                MessageOrigin::Typed
            } else {
                MessageOrigin::ChildTask
            });
        }
    }
    let progress = ProgressWriter {
        out: options.out.clone(),
        telemetry: telemetry.clone(),
        started,
        threshold,
        messages_total: corpus.messages.len(),
        archive_normalization: archive_normalization.clone(),
        store: store.clone(),
        phases: Arc::default(),
        snapshots: Arc::default(),
    };
    progress
        .phases
        .lock()
        .unwrap()
        .seconds
        .insert("archive_setup", archive_setup_seconds);
    progress.write("running", 0, 0)?;
    let _heartbeat = progress.heartbeat();
    for message in &corpus.messages {
        if telemetry.lock().unwrap().guard().is_err() {
            imports.flush_profiled(&mut importer, &progress, compactions.len())?;
            stopped = telemetry
                .lock()
                .unwrap()
                .stopped
                .clone()
                .or(Some("Spend limit".into()));
            break;
        }
        {
            let _phase = progress.phase("canonical_serialization", history.len() + 1);
            imports.push(message)?;
        }
        history.push(message.clone());
        if !execution.optimized()
            || imports.is_full()
            || history.len() % PROGRESS_INTERVAL_MESSAGES == 0
        {
            imports.flush_profiled(&mut importer, &progress, compactions.len())?;
        }
        if history.len() % PROGRESS_INTERVAL_MESSAGES == 0 {
            progress.write("running", imports.committed_messages, compactions.len())?;
        }
        {
            let _phase = progress.phase("masking_token_estimation", history.len());
            let tokens = estimator.message_tokens(message);
            worker_gate.observe(message, tokens)?;
            active_tokens += tokens;
        }
        if is_typed_user(message) {
            view_tokens_per_turn
                .push(json!({"message_id":message.id,"step":history.len(),"tokens":active_tokens}));
        }
        for call in &message.tool_calls {
            pending_calls.insert(call.id.clone());
        }
        if let Some(id) = &message.tool_call_id {
            pending_calls.remove(id);
        }
        activated |= active_tokens >= threshold.div_ceil(2);
        let ready = {
            let _phase = progress.phase("worker_compaction", history.len());
            if !activated {
                Ok(false)
            } else if !execution.optimized() {
                Ok(true)
            } else if options.context_strategy == ContextStrategy::Classical {
                Ok(worker_gate.needs_classical())
            } else {
                worker_gate.needs_lcm(&history, &pending_calls)
            }
        };
        let worker_result = match ready {
            Ok(false) => Ok(()),
            Ok(true) => {
                imports.flush_profiled(&mut importer, &progress, compactions.len())?;
                let _phase = progress.phase("worker_compaction", history.len());
                let result = if options.context_strategy == ContextStrategy::Classical {
                    worker.precompute(&session.id).await
                } else {
                    worker.catch_up_until(&session.id, history.len()).await
                };
                worker_gate.refresh(&store, &session.id, &history, options.context_strategy)?;
                result
            }
            Err(error) => {
                imports.flush_profiled(&mut importer, &progress, compactions.len())?;
                Err(error)
            }
        };
        if worker_result.is_err() {
            failures.push(json!({"step":history.len(),"phase":"worker","error":"Episode worker model or coverage failure"}));
            stopped = telemetry.lock().unwrap().stopped.clone().or(Some(
                "Episode operation failed after its bounded retry policy".into(),
            ));
            break;
        }
        // Preserve the per-message worker timeline: skip only planner-proven
        // no-ops. Every real operation observes exactly this committed prefix.
        // Main-model preparation still waits for complete tool rounds.
        let turn = turn_steps
            .iter()
            .position(|step| *step == history.len())
            .map(|index| index + 1);
        let checkpoint = turn
            .and_then(|turn| {
                options
                    .checkpoints_at_turns
                    .iter()
                    .position(|position| *position == turn)
            })
            .map(|index| index + 1);
        if (active_tokens < threshold || !pending_calls.is_empty())
            && (checkpoint.is_none() || !pending_calls.is_empty())
        {
            if let Some(turn) = turn
                && (options.capture_turn_frontiers || checkpoint.is_some())
            {
                imports.flush_profiled(&mut importer, &progress, compactions.len())?;
                let snapshot = turn_snapshot(
                    &store,
                    &session.id,
                    &history,
                    turn,
                    compactions.len(),
                    active_tokens,
                    !pending_calls.is_empty(),
                )?;
                if let Some(checkpoint) = checkpoint {
                    plan_mismatches.push(json!({"checkpoint":checkpoint,"turn":turn,"reason":"incomplete_tool_round","actual":snapshot}));
                }
                turn_frontiers.push(snapshot);
            }
            continue;
        }
        // Most imported messages need no context preparation. Avoid copying
        // the growing transcript until compaction or a probe checkpoint needs it.
        imports.flush_profiled(&mut importer, &progress, compactions.len())?;
        let preparation_phase = progress.phase("worker_compaction", history.len());
        let request = ModelRequest {
            output_schema: None,
            reasoning_effort: None,
            model: corpus
                .recorded_model
                .clone()
                .unwrap_or_else(|| "recorded-unknown".into()),
            system_prompt: corpus.instructions.clone(),
            messages: history.clone(),
            tools: vec![],
            disabled_tools: vec![],
            max_output_tokens: None,
            session_id: String::new(),
            ..Default::default()
        };
        let should_compact = active_tokens >= threshold;
        let active = estimator
            .prepare_step(&session.id, request, should_compact, &context_state)
            .await;
        drop(preparation_phase);
        // Preparation can persist a checkpoint/coverage before a fit error.
        worker_gate.refresh(&store, &session.id, &history, options.context_strategy)?;
        let prepared = match active {
            Ok(prepared) => prepared,
            Err(_) => {
                failures.push(json!({"step":history.len(),"phase":"compaction","error":"Real compaction could not fit or catch up"}));
                if let Some(reason) = &telemetry.lock().unwrap().stopped {
                    stopped = Some(reason.clone());
                    break;
                }
                if let Some(turn) = turn
                    && (options.capture_turn_frontiers || checkpoint.is_some())
                {
                    imports.flush_profiled(&mut importer, &progress, compactions.len())?;
                    let mut snapshot = turn_snapshot(
                        &store,
                        &session.id,
                        &history,
                        turn,
                        compactions.len(),
                        active_tokens,
                        false,
                    )?;
                    snapshot["status"] = json!("preparation_failed");
                    if let Some(checkpoint) = checkpoint {
                        plan_mismatches.push(json!({"checkpoint":checkpoint,"turn":turn,"reason":"preparation_failed","actual":snapshot}));
                    }
                    turn_frontiers.push(snapshot);
                }
                continue;
            }
        };
        #[cfg(test)]
        if let Some(trace) = prepared_trace {
            trace
                .lock()
                .unwrap()
                .push((history.len(), prepared.request.clone()));
        }
        if should_compact {
            let _phase = progress.phase("masking_token_estimation", history.len());
            active_tokens = estimator.estimated_tokens(&prepared.request);
        }
        if is_typed_user(message)
            && let Some(turn) = view_tokens_per_turn.last_mut()
        {
            turn["tokens"] = json!(active_tokens);
        }
        if prepared.notice.is_some() {
            let _phase = progress.phase("view_artifacts", history.len());
            let view = store
                .context_view(&session.id)?
                .context("Compaction did not persist a view")?;
            if options.capture_turn_frontiers {
                compactions.push(json!({"step":history.len(),"frontier":view.frontier,"frontier_message_id":store.context_view_frontier_message_id(&session.id,view.frontier)?,"tokens":view.tokens,"budget":view.budget,"threshold":threshold,"artifact_basis":"Compact planner snapshot; full serving-view dumps omitted"}));
            } else {
                let file = format!("view-{:03}.txt", compactions.len() + 1);
                let mut text = format!("[instructions]\n{}\n", prepared.request.system_prompt);
                let mut items = vec![
                    json!({"item":"instructions","tokens":token_count(&prepared.request.system_prompt)?}),
                ];
                for item in &prepared.request.messages {
                    let kind = if view.prefix.iter().any(|m| m.id == item.id)
                        && history.iter().any(|m| m.id == item.id)
                    {
                        "latest_typed_request"
                    } else if history.iter().any(|m| m.id == item.id) {
                        "raw"
                    } else if item.content.starts_with("Subject chapter index") {
                        "chapter_index"
                    } else if item.content.starts_with("Closed chapter summaries") {
                        "closed_chapter_summaries"
                    } else if item.content.starts_with("Open chapter episodes") {
                        "open_episode_summaries"
                    } else if item.content.contains("tools.history_search") {
                        "retrieval_instruction"
                    } else {
                        "runtime_note_or_legacy"
                    };
                    text.push_str(&format!(
                        "\n[{} {:?}]\n{}\n",
                        item.id, item.role, item.content
                    ));
                    if !item.tool_calls.is_empty() {
                        text.push_str(&serde_json::to_string(&item.tool_calls)?);
                        text.push('\n');
                    }
                    items.push(json!({"item":kind,"message_id":item.id,"tokens":estimator.message_tokens(item),"content_tokens":token_count(&item.content)?}));
                }
                std::fs::write(options.out.join(&file), text)?;
                items.push(
                    json!({"item":"request_overhead_and_tools","tokens":token_count("[]")? + 8}),
                );
                compactions.push(json!({"step":history.len(),"frontier":view.frontier,"frontier_message_id":store.context_view_frontier_message_id(&session.id, view.frontier)?,"tokens":view.tokens,"budget":view.budget,"threshold":threshold,"view_file":file,"items":items,"masked_tool_outputs":view.masked_tool_output_ids.len(),"clipped_tool_calls":view.clipped_tool_call_ids.len(),"masked_tool_output_ids":view.masked_tool_output_ids,"clipped_tool_call_ids":view.clipped_tool_call_ids}));
            }
        }
        progress.counts(
            imports.committed_messages,
            compactions.len(),
            imports.committed_transactions,
        );
        if should_compact {
            progress.write("running", imports.committed_messages, compactions.len())?;
        }
        if let Some(turn) = turn
            && (options.capture_turn_frontiers || checkpoint.is_some())
        {
            let mut snapshot = turn_snapshot(
                &store,
                &session.id,
                &history,
                turn,
                compactions.len(),
                active_tokens,
                false,
            )?;
            snapshot["checkpoint"] = json!(checkpoint);
            turn_frontiers.push(snapshot);
        }
        if let Some(checkpoint) = checkpoint {
            turn_frontiers.last_mut().unwrap()["frozen_view_sha256"] =
                json!(crate::probes::fingerprint(&prepared.request)?);
            let view = store.context_view(&session.id)?;
            let frontier = view.as_ref().and_then(|view| view.frontier);
            let frontier_id = store.context_view_frontier_message_id(&session.id, frontier)?;
            let planned = pinned_plan
                .as_ref()
                .map(|(plan, _)| &plan.checkpoints[checkpoint - 1]);
            let strategy = serde_json::to_value(options.context_strategy)?;
            let arm = strategy.as_str().unwrap();
            let mismatch = planned.is_some_and(|planned| {
                checkpoint_mismatch(planned, arm, &probes, &history, frontier_id.as_deref())
            });
            if let Some(planned) = planned {
                let actual_ids: Vec<_> = planned
                    .common_probe_ids
                    .iter()
                    .filter(|id| {
                        crate::probes::eligible(
                            probes.iter().find(|probe| &probe.id == *id).unwrap(),
                            checkpoint,
                            turn.unwrap(),
                            &history,
                            frontier_id.as_deref(),
                        )
                    })
                    .collect();
                let covariates = json!({"planned_arm_frontier":planned.arm_frontiers[arm],"proposed_common_probe_ids":planned.common_probe_ids,"actual_eligible_common_probe_ids":actual_ids,"actual_frontier_message_id":frontier_id,"actual_compaction_count":compactions.len()});
                turn_frontiers.last_mut().unwrap()["comparison_plan"] = covariates.clone();
                if mismatch {
                    plan_mismatches.push(json!({"checkpoint":checkpoint,"turn":turn,"reason":"plan_frontier_or_eligibility_mismatch","covariates":covariates}));
                }
            }
            let mut asked = 0;
            let mut delivered_workers = Vec::new();
            for probe in &probes {
                if !crate::probes::at_position(probe, turn.unwrap()) {
                    continue;
                }
                let mut row = json!({"id":probe.id,"type":probe.kind,"question":probe.question,"expected_answer":probe.expected_answer,"checkpoint":checkpoint,"turn":turn,"end_step":history.len(),"end_message_id":history.last().map(|message|&message.id),"compaction_count":compactions.len(),"frontier":frontier,"frontier_message_id":frontier_id,"evidence_message_ids":probe.evidence_message_ids,"earliest_checkpoint":probe.earliest_checkpoint});
                let metadata = serde_json::to_value(probe)?;
                for key in [
                    "ask_at_turn",
                    "origin",
                    "original_question",
                    "source_message_id",
                    "source_turn",
                ] {
                    if let Some(value) = metadata.get(key) {
                        row[key] = value.clone();
                    }
                }
                let reason = if planned
                    .is_some_and(|planned| !planned.common_probe_ids.contains(&probe.id))
                {
                    Some("excluded_by_common_plan")
                } else if mismatch {
                    Some("plan_frontier_mismatch")
                } else if !crate::probes::eligible(
                    probe,
                    checkpoint,
                    turn.unwrap(),
                    &history,
                    frontier_id.as_deref(),
                ) {
                    Some(if checkpoint < probe.earliest_checkpoint {
                        "before_earliest_checkpoint"
                    } else {
                        "evidence_not_compacted"
                    })
                } else if options.probe_limit.is_some_and(|limit| asked >= limit) {
                    Some("probe_limit")
                } else if telemetry.lock().unwrap().guard().is_err() {
                    Some("spend_guard")
                } else {
                    None
                };
                if let Some(reason) = reason {
                    row["status"] = json!("skipped");
                    row["skip_reason"] = json!(reason);
                    probe_rows.push(row);
                    continue;
                }
                asked += 1;
                let call_start = telemetry.lock().unwrap().calls.len();
                let answer = crate::probes::answer_with_policy(
                    probe_model.clone(),
                    &store,
                    &session.id,
                    &prepared.request,
                    probe,
                    options.agents.clone(),
                )
                .await;
                for (key, value) in answer.as_object().unwrap() {
                    row[key] = value.clone();
                }
                let call_end = telemetry.lock().unwrap().calls.len();
                row["probe_usage"] =
                    call_totals(&telemetry.lock().unwrap().calls[call_start..call_end]);
                if let Some(text) = row["answer"].as_str() {
                    // Judges use a stateless prompt without a worker session.
                    // Their paid request belongs to the source replay chat.
                    let (judgment, records) = bubblepaw_core::capture_request_usage_with_store(
                        &store,
                        &session.id,
                        None,
                        bubblepaw_core::RequestKind::Main,
                        crate::probes::judge_for_session(
                            probe_model.as_ref(),
                            probe,
                            text,
                            &session.id,
                        ),
                    )
                    .await;
                    for record in records {
                        store.queue_request_usage(record)?;
                    }
                    store.flush_request_usage(&session.id)?;
                    row["judge"] = judgment;
                    row["judge_usage"] = call_totals(&telemetry.lock().unwrap().calls[call_end..]);
                }
                probe_rows.push(row);
                delivered_workers.push(answer);
            }
            // Durable result snapshots after each checkpoint; paid answers never
            // enter the source chat; workers retain their rounds until acknowledgement.
            std::fs::write(
                options.out.join("probes.json"),
                serde_json::to_vec_pretty(&probe_rows)?,
            )?;
            for answer in delivered_workers {
                answer.acknowledge().await?;
            }
            if let Some(reason) = &telemetry.lock().unwrap().stopped {
                stopped = Some(reason.clone());
                break;
            }
        }
    }
    imports.flush_profiled(&mut importer, &progress, compactions.len())?;
    let final_view = if stopped.is_none() && pending_calls.is_empty() {
        let final_phase = progress.phase("worker_compaction", history.len());
        let prepared = estimator
            .prepare_fresh(
                &session.id,
                ModelRequest {
                    output_schema: None,
                    reasoning_effort: None,
                    model: corpus
                        .recorded_model
                        .clone()
                        .unwrap_or_else(|| "recorded-unknown".into()),
                    system_prompt: corpus.instructions.clone(),
                    messages: history.clone(),
                    tools: vec![],
                    disabled_tools: vec![],
                    max_output_tokens: None,
                    session_id: String::new(),
                    ..Default::default()
                },
            )
            .await?;
        ensure!(
            prepared.notice.is_none(),
            "Final view unexpectedly required another compaction"
        );
        #[cfg(test)]
        if let Some(trace) = prepared_trace {
            trace
                .lock()
                .unwrap()
                .push((history.len(), prepared.request.clone()));
        }
        drop(final_phase);
        let _phase = progress.phase("view_artifacts", history.len());
        let mut text = format!("[instructions]\n{}\n", prepared.request.system_prompt);
        for message in &prepared.request.messages {
            text.push_str(&format!(
                "\n[{} {:?}]\n{}\n",
                message.id, message.role, message.content
            ));
            if !message.tool_calls.is_empty() {
                text.push_str(&serde_json::to_string(&message.tool_calls)?);
                text.push('\n');
            }
        }
        if !options.capture_turn_frontiers {
            std::fs::write(options.out.join("final-view.txt"), text)?;
        }
        json!({"file":if options.capture_turn_frontiers {None}else{Some("final-view.txt")},"artifact_basis":if options.capture_turn_frontiers {"Full view archives omitted; checkpoint frozen request hashes and compact frontier metrics retained"}else{"Human-readable view archive"},"tokens":estimator.estimated_tokens(&prepared.request),"messages":prepared.request.messages.len(),"canonical_messages":history.len(),"extra_model_calls":0})
    } else {
        json!({"status":"unavailable","reason":"Replay stopped or final recorded tool round is incomplete; compaction snapshots remain available"})
    };
    if let Some(task) = relay_task {
        task.abort();
    }
    let episodes = store.context_episodes(&session.id)?;
    let chapters = store.context_subject_chapters(&session.id)?;
    let decisions = store.context_decisions(&session.id)?;
    let decision_rows: Vec<Value> = decisions.iter().map(|decision| {
        let mut row=serde_json::to_value(decision).unwrap();
        row["options"]=json!(decision.option_message_ids.iter().map(|id| {
            let message=history.iter().find(|m|&m.id==id);
            json!({"message_id":id,"user_excerpt":message.map(|m|m.content.chars().take(800).collect::<String>())})
        }).chain(std::iter::once(json!({"message_id":null,"option":"none"}))).collect::<Vec<_>>());
        row
    }).collect();
    let positions: HashMap<&str, usize> = history
        .iter()
        .enumerate()
        .map(|(i, m)| (m.id.as_str(), i))
        .collect();
    let chapter_start: HashMap<&str, String> = chapters
        .iter()
        .filter_map(|c| {
            let episode = episodes
                .iter()
                .find(|e| e.ordinal == c.first_episode_ordinal)?;
            let start = *positions.get(episode.first_message_id.as_str())?;
            history[..=start]
                .iter()
                .rev()
                .find(|m| is_typed_user(m))
                .map(|m| (c.id.as_str(), m.id.clone()))
        })
        .collect();
    let materialized_boundaries: Vec<String> = chapters
        .iter()
        .skip(1)
        .filter_map(|chapter| {
            episodes
                .iter()
                .find(|episode| episode.ordinal == chapter.first_episode_ordinal)
                .map(|episode| episode.first_message_id.clone())
        })
        .collect();
    let predicted = accepted_boundaries(&decisions, &history);
    let unmaterialized_boundaries: Vec<&String> = predicted
        .iter()
        .filter(|id| !materialized_boundaries.contains(id))
        .collect();
    let chapter_rows: Vec<Value> = chapters
        .iter()
        .map(|chapter| {
            let first = episodes
                .iter()
                .find(|e| e.ordinal == chapter.first_episode_ordinal);
            let last = episodes
                .iter()
                .find(|e| e.ordinal == chapter.last_episode_ordinal);
            let mut row = serde_json::to_value(chapter).unwrap();
            row["first_message_id"] = json!(first.map(|e| &e.first_message_id));
            row["last_message_id"] = json!(last.map(|e| &e.last_message_id));
            row["start_record_index"] =
                json!(first.and_then(|e| positions.get(e.first_message_id.as_str())));
            row["end_record_index"] =
                json!(last.and_then(|e| positions.get(e.last_message_id.as_str())));
            row["start_typed_record"] = json!(chapter_start.get(chapter.id.as_str()));
            row
        })
        .collect();
    let boundary_scores = corpus.boundary_gold.as_ref().map(|gold| {
        let starts = string_array(&gold["gold"]); let ambiguous = string_array(&gold["ambiguous"]);
        let typed: HashMap<&str,usize> = corpus.messages.iter().filter(|m| is_typed_user(m)).enumerate().map(|(i,m)| (m.id.as_str(),i)).collect();
        json!({"source":gold["source"],"predicted":predicted,"gold":starts,"accepted_boundary_count":predicted.len(),"materialized_chapter_count":chapters.len(),"materialized_boundaries":materialized_boundaries,"unmaterialized_boundaries":unmaterialized_boundaries,"exact":boundary_score(&predicted,&starts,&typed,&ambiguous,false),"tolerant":boundary_score(&predicted,&starts,&typed,&ambiguous,true),"prediction_basis":"Unique persisted accepted episode-start choices in canonical order, excluding the initial typed chat-start record. Decisions follow episode summarization and may mark a phase transition during continuing agent work.","definition":"pipeline_trial.py maximum one-to-one cardinality, typed distance <= 1 or predicted ambiguous; ambiguous records can match any gold once"})
    }).unwrap_or_else(|| json!({"status":"unavailable","reason":"No verified boundary gold exists for this chat; template labels are not gold","predicted":predicted,"accepted_boundary_count":predicted.len(),"materialized_chapter_count":chapters.len(),"unmaterialized_boundaries":unmaterialized_boundaries}));
    let mut predicted_related = Vec::new();
    for chapter in &chapters {
        for (i, target) in chapter.related_ids.iter().enumerate() {
            let evidence = chapter.related_evidence.get(i);
            predicted_related.push(json!({"source_chapter":chapter.id,"target_chapter":target,"source_start_record":chapter_start.get(chapter.id.as_str()),"target_start_record":chapter_start.get(target.as_str()),"quote":evidence.map(|e| &e.quote),"quote_matched":evidence.and_then(|e|e.related_quote_matched)}));
        }
    }
    let related = related_score(&corpus, &history, &episodes, &chapters, &predicted_related);
    progress.write("loop_finished", history.len(), compactions.len())?;
    let meter = telemetry.lock().unwrap();
    let episode_rows: Vec<Value> = episodes.iter().map(|e| json!({"episode":e,"kind":meter.episode_kinds.get(&e.first_message_id),"start_record_index":positions.get(e.first_message_id.as_str()),"end_record_index":positions.get(e.last_message_id.as_str()),"summary_tokens":token_count(&e.summary).unwrap_or(0),"summary_chars":e.summary.chars().count()})).collect();
    let mut by_kind = BTreeMap::<String, Value>::new();
    let mut input = 0_u64;
    let mut output = 0_u64;
    let mut reasoning = 0_u64;
    let mut unknown_reasoning = 0;
    for call in &meter.calls {
        let kind = call["kind"].as_str().unwrap_or("unknown").to_string();
        let entry = by_kind
            .entry(kind)
            .or_insert(json!({"calls":0,"input_tokens":0,"output_tokens":0,"cache_read_tokens":0,"cache_creation_tokens":0,"reasoning_tokens":0}));
        entry["calls"] = json!(entry["calls"].as_u64().unwrap() + 1);
        for name in [
            "input_tokens",
            "output_tokens",
            "cache_read_tokens",
            "cache_creation_tokens",
            "reasoning_tokens",
        ] {
            entry[name] = json!(entry[name].as_u64().unwrap() + call[name].as_u64().unwrap_or(0));
        }
        input += call["input_tokens"].as_u64().unwrap_or(0);
        output += call["output_tokens"].as_u64().unwrap_or(0);
        reasoning += call["reasoning_tokens"].as_u64().unwrap_or(0);
        unknown_reasoning += usize::from(call["reasoning_tokens"].is_null());
    }
    let storage = store.storage_stats()?;
    let import_transactions = imports.committed_transactions;
    let store_transactions = storage
        .transaction_count
        .saturating_sub(import_transactions);
    let mut report = json!({"chat":corpus.chat_id,"mode":if options.dry_run {"scripted"} else {"live"},"session_id":session.id,"database":"store","data_dir":"store","recorded_model":corpus.recorded_model,"context_window":corpus.context_window,"threshold":threshold,"status":if stopped.is_some() {"stopped"} else {"complete"},"stop_reason":stopped,"messages_replayed":history.len(),"messages_total":corpus.messages.len(),"episodes":episode_rows,"chapters":chapter_rows,"decisions":decision_rows,"compactions":compactions,"final_view":final_view,"boundary_scores":boundary_scores,"related":related,"totals":{"calls_by_kind":by_kind,"input_tokens":input,"output_tokens":output,"reasoning_tokens":reasoning,"reasoning_usage_unavailable_calls":unknown_reasoning,"flash_estimated_cost_usd":meter.flash_cost_usd,"jev_reported_cost_usd":meter.jev_cost_usd,"haiku_estimated_cost_usd":meter.haiku_cost_usd,"model_estimated_cost_usd":meter.flash_cost_usd+meter.haiku_cost_usd+meter.openai_cost_usd,"estimated_cost_usd":meter.flash_cost_usd+meter.jev_cost_usd+meter.openai_cost_usd+meter.haiku_cost_usd,"reserved_unknown_cost_usd":meter.reserved_unknown_cost_usd,"price_basis":PRICE_BASIS,"wall_seconds":started.elapsed().as_secs_f64(),"failures":failures.len(),"physical_failed_attempts":meter.failures,"logical_failures":failures.len(),"retries":meter.retries},"calls":meter.calls,"failures":failures,"storage":storage,"storage_transaction_basis":{"store_transactions":store_transactions,"canonical_import_transactions":import_transactions,"schema_initialization_excluded":true},"provenance":corpus.provenance,"limitations":["Recorded canonical history replay, no main-model continuation; optional isolated recall probes are not downstream task acceptance","Original recorded tools are never executed","Last incomplete pack and open chapter remain pending, as in production","Input estimates use production o200k estimator without live main-provider calibration","Unknown provider usage remains reserved and is not reported as measured spend"]});
    if meter.calls.iter().any(|call| {
        call["model_address"]
            .as_str()
            .is_some_and(|address| address.starts_with("openai/"))
    }) {
        report["totals"]["openai_estimated_cost_usd"] = json!(meter.openai_cost_usd);
        report["totals"]["price_basis"] = json!(PRICE_BASIS);
    }
    report["phase_timings"] = progress.timings();
    report["archive_index"] = json!({"name":"messages_id","setup_seconds":archive_setup_seconds,"scope":"Production schema message-ID index; migration excluded from application/import transaction counters; index maintenance included in canonical commits"});
    report["context_strategy"] = serde_json::to_value(options.context_strategy)?;
    report["summary_input"] = serde_json::to_value(episode_policy.summary_input)?;
    report["totals"]["cache_read_tokens"] = json!(
        meter
            .calls
            .iter()
            .filter_map(|call| call["cache_read_tokens"].as_u64())
            .sum::<u64>()
    );
    report["totals"]["cache_creation_tokens"] = json!(
        meter
            .calls
            .iter()
            .filter_map(|call| call["cache_creation_tokens"].as_u64())
            .sum::<u64>()
    );
    report["compaction_count"] = json!(compactions.len());
    report["view_tokens_per_turn"] = json!(view_tokens_per_turn);
    report["rolling_checkpoint"] = serde_json::to_value(store.context_checkpoint(&session.id)?)?;
    report["partial"] = json!(stopped.is_some() || !plan_mismatches.is_empty());
    if stopped.is_none() && !plan_mismatches.is_empty() {
        report["status"] = json!("partial");
    }
    report["max_cost_usd"] = json!(meter.max_cost_usd);
    report["corpus_sha256"] = json!(corpus_sha256);
    report["source_messages_total"] = json!(archive_normalization.source_messages_total);
    let marker_count = archive_normalization.markers_replayed(history.len());
    report["source_messages_replayed"] = json!(history.len() - marker_count);
    report["archive_unknown_outcome_markers"] = json!(marker_count);
    report["archive_unknown_outcome_markers_total"] =
        json!(archive_normalization.marker_steps.len());
    report["archive_normalization"] = archive_normalization.metadata();
    report["turn_frontiers"] = json!(turn_frontiers);
    report["probe_plan"] = json!(
        pinned_plan
            .as_ref()
            .map(|(plan, digest)| json!({"plan_sha256":digest,"plan":plan}))
    );
    report["probes"] = json!({"metadata":crate::probes::metadata_for(&probe_model.model, probe_model.effort),"file":options.probes,"probe_set_sha256":crate::probes::fingerprint(&probes)?,"checkpoints_at_turns":options.checkpoints_at_turns,"checkpoint_semantics":"End of complete typed-user turn K; ask_at_turn fixes an individual probe position; earliest_checkpoint is its ordinal in the configured position list","probe_limit_per_checkpoint":pinned_plan.as_ref().map(|(plan,_)|plan.probe_limit_per_checkpoint).unwrap_or(options.probe_limit),"rows":probe_rows,"groups":crate::probes::aggregate(&probe_rows,serde_json::to_value(options.context_strategy)?.as_str().unwrap()),"unreached_turn_checkpoints":options.checkpoints_at_turns.iter().filter(|turn| !turn_frontiers.iter().any(|row|row["turn"].as_u64()==Some(**turn as u64))).collect::<Vec<_>>(),"plan_mismatches":plan_mismatches,"scripted_grades_are_synthetic":options.dry_run});
    report["totals"]["probe"] = call_totals(
        &meter
            .calls
            .iter()
            .filter(|call| call["kind"] == "probe")
            .cloned()
            .collect::<Vec<_>>(),
    );
    report["totals"]["judge"] = call_totals(
        &meter
            .calls
            .iter()
            .filter(|call| call["kind"] == "judge")
            .cloned()
            .collect::<Vec<_>>(),
    );
    report["totals"]["summarization"] = call_totals(
        &meter
            .calls
            .iter()
            .filter(|call| matches!(call["kind"].as_str(), Some("summary" | "chapter")))
            .cloned()
            .collect::<Vec<_>>(),
    );
    report["totals"]["decision"] = call_totals(
        &meter
            .calls
            .iter()
            .filter(|call| call["kind"] == "decision")
            .cloned()
            .collect::<Vec<_>>(),
    );
    report["totals"]["projected_paid_cost_usd"] = json!(
        meter
            .calls
            .iter()
            .filter_map(|call| call["projected_paid_cost_usd"]
                .as_f64()
                .or_else(|| call["cost_usd"].as_f64()))
            .sum::<f64>()
    );
    report["totals"]["projection_basis"] = json!(format!(
        "Scripted O200k estimates use legacy Chat framing, including for Haiku; synthetic output sizes and no cache discounts, no paid usage or exact provider token claim. Decisions projected as Luna input only. Live physical calls reserve the selected model worst input/cache-creation rate and configured maximum output. {}",
        PRICE_BASIS
    ));
    let summary_calls: Vec<_> = meter
        .calls
        .iter()
        .filter(|call| call["kind"] == "summary")
        .collect();
    report["totals"]["summary"] = json!({
        "calls":summary_calls.len(),
        "input_tokens":summary_calls.iter().filter_map(|call| call["input_tokens"].as_u64()).sum::<u64>(),
        "output_tokens":summary_calls.iter().filter_map(|call| call["output_tokens"].as_u64()).sum::<u64>(),
        "estimated_cost_usd":summary_calls.iter().filter_map(|call| call["cost_usd"].as_f64()).sum::<f64>(),
        "token_basis":if options.dry_run { "Scripted O200k request estimates and synthetic output sizes; no provider calls or measured usage" }
            else { "Provider-reported attempt usage" },
    });
    if options.context_strategy == ContextStrategy::Classical {
        report["limitations"].as_array_mut().unwrap().push(json!(
            "Files touched uses recorded structured read/edit/write path arguments. Opaque JavaScript source and transient nested reads do not provide a persisted file trail."
        ));
    }
    std::fs::write(
        options.out.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    std::fs::write(options.out.join("report.md"), markdown(&report)?)?;
    Ok(report)
}

/// Metadata-only progress, so a replay that times out still reports its counts.
/// report.json remains the only completed-run artifact; this is a diagnostic.
const PROGRESS_INTERVAL_MESSAGES: usize = 100;

#[derive(Clone)]
struct ProgressWriter {
    out: PathBuf,
    telemetry: Arc<Mutex<Telemetry>>,
    started: Instant,
    threshold: usize,
    messages_total: usize,
    archive_normalization: ArchiveNormalization,
    store: Store,
    phases: Arc<Mutex<ReplayPhaseTimings>>,
    snapshots: Arc<Mutex<ReplayProgressSnapshot>>,
}

#[derive(Default)]
struct ReplayPhaseTimings {
    seconds: BTreeMap<&'static str, f64>,
    current: Option<(&'static str, usize, Instant)>,
    canonical_store_writes_seconds: f64,
}

struct ReplayPhase {
    phases: Arc<Mutex<ReplayPhaseTimings>>,
}

impl Drop for ReplayPhase {
    fn drop(&mut self) {
        let mut phases = self.phases.lock().unwrap();
        if let Some((name, _, began)) = phases.current.take() {
            *phases.seconds.entry(name).or_default() += began.elapsed().as_secs_f64();
        }
    }
}

#[derive(Default)]
struct ReplayProgressSnapshot {
    status: String,
    messages_imported: usize,
    compactions: usize,
    import_transactions: u64,
}

struct ReplayHeartbeat {
    stop: std::sync::mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ReplayHeartbeat {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl ProgressWriter {
    fn phase(&self, name: &'static str, step: usize) -> ReplayPhase {
        let mut phases = self.phases.lock().unwrap();
        debug_assert!(phases.current.is_none(), "Replay phases must not nest");
        phases.current = Some((name, step, Instant::now()));
        ReplayPhase {
            phases: self.phases.clone(),
        }
    }

    fn counts(&self, messages: usize, compactions: usize, transactions: u64) {
        let mut snapshot = self.snapshots.lock().unwrap();
        snapshot.messages_imported = messages;
        snapshot.compactions = compactions;
        snapshot.import_transactions = transactions;
    }

    fn timings(&self) -> Value {
        let phases = self.phases.lock().unwrap();
        let mut seconds = phases.seconds.clone();
        let current = phases.current.map(|(name, step, began)| {
            let elapsed = began.elapsed().as_secs_f64();
            *seconds.entry(name).or_default() += elapsed;
            json!({"phase": name, "step": step, "elapsed_seconds": elapsed})
        });
        let importer_seconds = seconds
            .get("canonical_serialization")
            .copied()
            .unwrap_or(0.0)
            + seconds.get("canonical_commit").copied().unwrap_or(0.0);
        seconds.insert("canonical_import", importer_seconds);
        let store_seconds = self.store.write_seconds();
        let canonical_store_seconds = phases.canonical_store_writes_seconds;
        json!({
            "seconds": seconds,
            "current_operation": current,
            "store_writes_seconds": store_seconds,
            "canonical_store_writes_seconds": canonical_store_seconds,
            "internal_store_writes_seconds": (store_seconds - canonical_store_seconds).max(0.0),
            "basis": "Cumulative wall time, current operation included. canonical_import is canonical_serialization plus canonical_commit; worker_compaction includes planning, tokenization, model waits and internal Store writes. store_writes_seconds is the Store write-boundary counter, counted once, including canonical imports; canonical_store_writes_seconds records the counter delta around each profiled import, and internal_store_writes_seconds is the remainder. canonical_commit also includes importer overhead outside Store writes. Store timing excludes event journaling and schema initialization; uninstrumented time is not attributed."
        })
    }

    fn heartbeat(&self) -> ReplayHeartbeat {
        let writer = self.clone();
        let (stop, receiver) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            while matches!(
                receiver.recv_timeout(Duration::from_secs(1)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ) {
                // Best-effort diagnostic; foreground publications still surface errors.
                let _ = writer.persist();
            }
        });
        ReplayHeartbeat {
            stop,
            thread: Some(thread),
        }
    }

    /// Writes to a temporary file and renames it, so a reader never sees a torn file.
    fn write(&self, status: &str, messages_imported: usize, compactions: usize) -> Result<()> {
        {
            let mut snapshot = self.snapshots.lock().unwrap();
            snapshot.status = status.into();
            snapshot.messages_imported = messages_imported;
            snapshot.compactions = compactions;
        }
        self.persist()
    }

    fn persist(&self) -> Result<()> {
        // Serialize foreground and heartbeat publications, including temp rename.
        let snapshot = self.snapshots.lock().unwrap();
        let timings = self.timings();
        let physical = physical_storage(&self.out.join("store"))?;
        let store_transactions = self
            .store
            .revision()
            .saturating_sub(snapshot.import_transactions);
        let progress = {
            let meter = self.telemetry.lock().unwrap();
            json!({
                "diagnostic": "Replay progress; not a completed report or common probe plan",
                "status": snapshot.status,
                "messages_imported": snapshot.messages_imported,
                "messages_total": self.messages_total,
                "source_messages_total": self.archive_normalization.source_messages_total,
                "source_messages_replayed": snapshot.messages_imported - self.archive_normalization.markers_replayed(snapshot.messages_imported),
                "archive_unknown_outcome_markers": self.archive_normalization.markers_replayed(snapshot.messages_imported),
                "archive_unknown_outcome_markers_total": self.archive_normalization.marker_steps.len(),
                "compactions_completed": snapshot.compactions,
                "phase_timings": timings,
                "storage": {
                    "database_bytes": physical["database_bytes"],
                    "chat_bytes": physical["chat_bytes"],
                    "serving_bytes": physical["serving_bytes"],
                    "catalog_bytes": physical["catalog_bytes"],
                    "wal_bytes": physical["wal_bytes"],
                    "store_transactions": store_transactions,
                    "canonical_import_transactions": snapshot.import_transactions,
                    "transaction_count": store_transactions + snapshot.import_transactions,
                    "basis": "Filesystem sizes and committed transaction counters; no SQL polling"
                },
                "resolved_threshold": self.threshold,
                "elapsed_seconds": self.started.elapsed().as_secs_f64(),
                "calls": meter.calls.len(),
                "failed_attempts": meter.failures,
                "projected_cost": {
                    "basis": "Scripted O200k estimates or provider-reported attempt usage; not a measured spend total",
                    "summary_and_chapter": projected_group(&meter.calls, &["summary", "chapter"]),
                    "decision": projected_group(&meter.calls, &["decision"]),
                },
            })
        };
        let temporary = self.out.join("progress.json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(&progress)?)?;
        std::fs::rename(&temporary, self.out.join("progress.json"))?;
        Ok(())
    }
}

fn projected_group(calls: &[Value], kinds: &[&str]) -> Value {
    let matching: Vec<_> = calls
        .iter()
        .filter(|call| {
            call["kind"]
                .as_str()
                .is_some_and(|kind| kinds.contains(&kind))
        })
        .collect();
    json!({
        "calls": matching.len(),
        "projected_cost_usd": matching
            .iter()
            .filter_map(|call| call["projected_paid_cost_usd"].as_f64().or_else(|| call["cost_usd"].as_f64()))
            .sum::<f64>(),
    })
}

fn token_count(text: &str) -> Result<usize> {
    // bpe_for_tokenizer uses tiktoken-rs's process-wide cached singleton.
    Ok(
        tiktoken_rs::bpe_for_tokenizer(tiktoken_rs::tokenizer::Tokenizer::O200kBase)?
            .encode_ordinary(text)
            .len(),
    )
}
fn role_input_tokens(system: &str, user: &str) -> Result<u64> {
    Ok(token_count(&json!({"model":"deepseek-v4.1-flash","messages":[{"role":"system","content":system},{"role":"user","content":user}],"max_tokens":65536,"stream":false,"reasoning_effort":"low","thinking":{"type":"enabled"}}).to_string())? as u64)
}
fn call_totals(calls: &[Value]) -> Value {
    json!({"calls":calls.len(),"input_tokens":calls.iter().filter_map(|call|call["input_tokens"].as_u64()).sum::<u64>(),"output_tokens":calls.iter().filter_map(|call|call["output_tokens"].as_u64()).sum::<u64>(),"cache_read_tokens":calls.iter().filter_map(|call|call["cache_read_tokens"].as_u64()).sum::<u64>(),"cache_creation_tokens":calls.iter().filter_map(|call|call["cache_creation_tokens"].as_u64()).sum::<u64>(),"cost_usd":calls.iter().filter_map(|call|call["cost_usd"].as_f64()).sum::<f64>(),"projected_paid_cost_usd":calls.iter().filter_map(|call|call["projected_paid_cost_usd"].as_f64().or_else(||call["cost_usd"].as_f64())).sum::<f64>()})
}

fn accepted_boundaries(
    decisions: &[bubblepaw_core::ContextDecision],
    history: &[Message],
) -> Vec<String> {
    let first_typed = history.iter().find(|m| is_typed_user(m)).map(|m| &m.id);
    let positions: HashMap<&String, usize> = history
        .iter()
        .enumerate()
        .map(|(i, m)| (&m.id, i))
        .collect();
    let mut boundaries: Vec<String> = decisions
        .iter()
        .filter_map(|decision| decision.choice.clone())
        .filter(|choice| Some(choice) != first_typed)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    boundaries.sort_by_key(|id| positions.get(id).copied().unwrap_or(usize::MAX));
    boundaries
}
fn string_array(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect()
}

fn augment(
    p: usize,
    candidates: &[Vec<usize>],
    assigned: &mut [Option<usize>],
    seen: &mut HashSet<usize>,
) -> bool {
    for &g in &candidates[p] {
        if !seen.insert(g) {
            continue;
        }
        if assigned[g].is_none() || augment(assigned[g].unwrap(), candidates, assigned, seen) {
            assigned[g] = Some(p);
            return true;
        }
    }
    false
}
pub fn boundary_score(
    predicted: &[String],
    gold: &[String],
    ordinals: &HashMap<&str, usize>,
    ambiguous: &[String],
    tolerant: bool,
) -> Value {
    let candidates: Vec<Vec<usize>> = predicted
        .iter()
        .map(|p| {
            let mut eligible: Vec<usize> = gold
                .iter()
                .enumerate()
                .filter(|(_, g)| {
                    p == *g
                        || (tolerant
                            && (ambiguous.contains(p)
                                || ordinals
                                    .get(p.as_str())
                                    .zip(ordinals.get(g.as_str()))
                                    .is_some_and(|(a, b)| a.abs_diff(*b) <= 1)))
                })
                .map(|(i, _)| i)
                .collect();
            eligible.sort_by_key(|&i| {
                (
                    ordinals
                        .get(p.as_str())
                        .zip(ordinals.get(gold[i].as_str()))
                        .map(|(a, b)| a.abs_diff(*b))
                        .unwrap_or(usize::MAX),
                    ordinals
                        .get(gold[i].as_str())
                        .copied()
                        .unwrap_or(usize::MAX),
                )
            });
            eligible
        })
        .collect();
    let mut assigned = vec![None; gold.len()];
    let mut seen_predictions = HashSet::new();
    for (p, id) in predicted.iter().enumerate() {
        if seen_predictions.insert(id) {
            augment(p, &candidates, &mut assigned, &mut HashSet::new());
        }
    }
    let tp = assigned.iter().flatten().count();
    let fp = predicted.len() - tp;
    let missed = gold.len() - tp;
    let matching: Vec<_> = assigned
        .iter()
        .enumerate()
        .filter_map(|(g, p)| p.map(|p| json!({"predicted":predicted[p],"gold":gold[g]})))
        .collect();
    let false_positive: Vec<_> = predicted
        .iter()
        .enumerate()
        .filter(|(i, _)| !assigned.contains(&Some(*i)))
        .map(|(_, p)| p)
        .collect();
    let missed_gold: Vec<_> = gold
        .iter()
        .enumerate()
        .filter(|(i, _)| assigned[*i].is_none())
        .map(|(_, g)| g)
        .collect();
    json!({"tp":tp,"fp":fp,"fn":missed,"precision":if tp+fp>0 {tp as f64/(tp+fp) as f64} else {f64::from(gold.is_empty())},"recall":if gold.is_empty(){1.0}else{tp as f64/gold.len() as f64},"f1":if 2*tp+fp+missed>0{2.0*tp as f64/(2*tp+fp+missed) as f64}else{1.0},"matching":matching,"false":false_positive,"missed":missed_gold})
}

fn related_score(
    corpus: &ReplayCorpus,
    history: &[Message],
    episodes: &[bubblepaw_core::ContextEpisode],
    chapters: &[bubblepaw_core::ContextSubjectChapter],
    predicted: &[Value],
) -> Value {
    let membership = |record: &str| -> Option<String> {
        let index = history.iter().position(|m| m.id == record)?;
        episodes
            .iter()
            .find(|e| {
                let first = history.iter().position(|m| m.id == e.first_message_id);
                let last = history.iter().position(|m| m.id == e.last_message_id);
                first
                    .zip(last)
                    .is_some_and(|(first, last)| first <= index && index <= last)
            })
            .map(|e| e.chapter_id.clone())
    };
    let aligned: Vec<Value> = corpus.relates_gold.iter().map(|gold| {
        let source=gold["source_start_record"].as_str().and_then(membership);let target=gold["target_start_record"].as_str().and_then(membership);
        let source_closed=source.as_ref().is_some_and(|id|chapters.iter().any(|c|&c.id==id && c.status==bubblepaw_core::SubjectChapterStatus::Closed));
        let found=source.as_ref().zip(target.as_ref()).is_some_and(|(s,t)|predicted.iter().any(|p|p["source_chapter"]==*s&&p["target_chapter"]==*t));
        json!({"gold":gold,"aligned_source_chapter":source,"aligned_target_chapter":target,"source_closed":source_closed,"found":found})
    }).collect();
    let tp = aligned.iter().filter(|g| g["found"] == true).count();
    let fp = predicted.len().saturating_sub(tp);
    let missed = aligned.len() - tp;
    let gold_chapters = corpus
        .boundary_gold
        .as_ref()
        .and_then(|g| g["chapters"].as_array());
    let mut rows = Vec::new();
    if let Some(gold_chapters) = gold_chapters {
        let labels: HashMap<String, Option<String>> = gold_chapters
            .iter()
            .filter_map(|g| {
                Some((
                    g["id"].as_str()?.to_owned(),
                    g["starts_at"].as_str().and_then(membership),
                ))
            })
            .collect();
        for (i, gold) in gold_chapters.iter().enumerate() {
            let label = gold["id"].as_str().unwrap_or("");
            let source = labels.get(label).and_then(|v| v.as_ref());
            let previous = i
                .checked_sub(1)
                .and_then(|i| gold_chapters[i]["id"].as_str());
            let mut any = HashSet::new();
            let mut found = HashSet::new();
            for link in predicted
                .iter()
                .filter(|p| source.is_some_and(|s| p["source_chapter"] == *s))
            {
                let target = link["target_chapter"].as_str().unwrap_or("");
                let targets: Vec<String> = labels
                    .iter()
                    .filter(|(_, chapter)| chapter.as_deref() == Some(target))
                    .map(|(label, _)| label.clone())
                    .collect();
                let targets = if targets.is_empty() {
                    vec![format!("unmapped:{target}")]
                } else {
                    targets
                };
                for target in targets {
                    any.insert(target.clone());
                    if link["quote_matched"] == true {
                        found.insert(target);
                    }
                }
            }
            let mut any: Vec<_> = any.into_iter().collect();
            any.sort();
            let mut found: Vec<_> = found.into_iter().collect();
            found.sort();
            let expected = gold["relates_to"]
                .as_str()
                .map(|s| vec![s.to_owned()])
                .unwrap_or_default();
            let closed = source.is_some_and(|s| {
                chapters
                    .iter()
                    .any(|c| &c.id == s && c.status == bubblepaw_core::SubjectChapterStatus::Closed)
            });
            rows.push(json!({"chapter":label,"gold":expected,"predicted":any,"quote_found_predicted":found,"previous":previous,"aligned_source_chapter":source,"source_closed":closed,"membership_ambiguous":source.is_some_and(|s|labels.values().filter(|v|v.as_ref()==Some(s)).count()>1)}));
        }
    }
    let full_metrics = json!({"G-any":related_metric(&rows,false,false,false),"G-return":related_metric(&rows,true,false,false)});
    let closed_metrics = json!({"G-any":related_metric(&rows,false,false,true),"G-return":related_metric(&rows,true,false,true)});
    let found_metrics = json!({"G-any":related_metric(&rows,false,true,false),"G-return":related_metric(&rows,true,true,false)});
    json!({"gold":aligned,"predicted":predicted,"tp":tp,"fp":fp,"fn":missed,"precision":if predicted.is_empty(){f64::from(aligned.is_empty())}else{tp as f64/predicted.len() as f64},"recall":if aligned.is_empty(){1.0}else{tp as f64/aligned.len() as f64},"f1":if 2*tp+fp+missed==0{1.0}else{2.0*tp as f64/(2*tp+fp+missed) as f64},"chapter_rows":rows,"C2_metrics":{"full_corpus":full_metrics,"closed_source_only":closed_metrics,"quote_found_only_full_corpus":found_metrics},"basis":"Gold chapter start records mapped to predicted episode membership; merged memberships are flagged and target labels retain every possible gold membership. Full corpus counts unobserved/open source chapters as missed; closed-source-only exposes observable scoring. G-return excludes immediate previous gold chapter from both sets. Quote-found-only is literal matching, not a semantic judge."})
}

fn related_metric(rows: &[Value], returns: bool, found_only: bool, closed_only: bool) -> Value {
    let mut tp = 0;
    let mut fp = 0;
    let mut missed = 0;
    let mut null_rows = 0;
    let mut false_rows = 0;
    let mut count = 0;
    for row in rows
        .iter()
        .filter(|r| !closed_only || r["source_closed"] == true)
    {
        count += 1;
        let previous = row["previous"].as_str();
        let gold: HashSet<String> = string_array(&row["gold"])
            .into_iter()
            .filter(|g| !returns || Some(g.as_str()) != previous)
            .collect();
        let predicted: HashSet<String> = string_array(
            &row[if found_only {
                "quote_found_predicted"
            } else {
                "predicted"
            }],
        )
        .into_iter()
        .filter(|p| !returns || Some(p.as_str()) != previous)
        .collect();
        tp += gold.intersection(&predicted).count();
        fp += predicted.difference(&gold).count();
        missed += gold.difference(&predicted).count();
        if gold.is_empty() {
            null_rows += 1;
            false_rows += usize::from(!predicted.is_empty());
        }
    }
    json!({"rows":count,"tp":tp,"fp":fp,"fn":missed,"precision":if tp+fp>0{Some(tp as f64/(tp+fp)as f64)}else{None},"recall":if tp+missed>0{Some(tp as f64/(tp+missed)as f64)}else{None},"false_link_gold_null_rows":false_rows,"gold_null_rows":null_rows,"false_link_rate":if null_rows>0{Some(false_rows as f64/null_rows as f64)}else{None}})
}

fn markdown(report: &Value) -> Result<String> {
    let mut text = format!(
        "# Rust context replay: {}\n\nStrategy: {}; mode: {}; status: {}; messages: {}/{}; threshold: {}.\n\n",
        report["chat"],
        report["context_strategy"],
        report["mode"],
        report["status"],
        report["messages_replayed"],
        report["messages_total"],
        report["threshold"]
    );
    text.push_str(&format!("| Episodes | Chapters | Compactions | Exact F1 | Tolerant F1 | Estimated USD | Input / output tokens |\n|---:|---:|---:|---:|---:|---:|---:|\n| {} | {} | {} | {} | {} | {} | {} / {} |\n\n",report["episodes"].as_array().map_or(0,Vec::len),report["chapters"].as_array().map_or(0,Vec::len),report["compactions"].as_array().map_or(0,Vec::len),report["boundary_scores"]["exact"]["f1"],report["boundary_scores"]["tolerant"]["f1"],report["totals"]["estimated_cost_usd"],report["totals"]["input_tokens"],report["totals"]["output_tokens"]));
    if let Some(groups) = report["probes"]["groups"].as_array() {
        text.push_str(&crate::probes::markdown(groups));
        text.push_str("\nScripted answers and grades prove plumbing only. Recall denominator contains valid judgments; skipped and failed probes remain explicit in JSON.\n\n");
    }
    for (title, key) in [
        (
            "Serving view tokens at typed user turns",
            "view_tokens_per_turn",
        ),
        ("Rolling checkpoint", "rolling_checkpoint"),
        ("Episodes", "episodes"),
        (
            "Decisions (options, averaged probabilities, choice)",
            "decisions",
        ),
        (
            "Chapters (ranges, description, Related quotes and match flags)",
            "chapters",
        ),
        (
            "Compactions (frontier, per-item tokens, masking and saved views)",
            "compactions",
        ),
        ("Boundary gold and exact/tolerant scores", "boundary_scores"),
        (
            "Final serving view (complete replayed history)",
            "final_view",
        ),
        ("Related gold and scores", "related"),
        ("Calls, tokens, peak-price cost and wall time", "totals"),
        (
            "Storage while replay connections are open (DB, WAL, committed transactions)",
            "storage",
        ),
        (
            "Retained storage after replay connections close",
            "final_artifact_storage",
        ),
        ("Transaction accounting", "storage_transaction_basis"),
        ("Failures and stop reason", "failures"),
        ("Corpus and label provenance", "provenance"),
        ("Unverified and limitations", "limitations"),
    ] {
        text.push_str(&format!(
            "## {title}\n\n```json\n{}\n```\n\n",
            serde_json::to_string_pretty(&report[key])?
        ));
    }
    if !report["stop_reason"].is_null() {
        text.push_str(&format!("Stop reason: {}\n", report["stop_reason"]));
    }
    Ok(text)
}

#[cfg(test)]
mod telemetry_tests;

#[cfg(test)]
mod tests {
    use super::*;

    mod http_probes;

    /// The embedded table plus one test-only decider, so fixtures price without private model ids.
    fn example_prices() -> &'static PriceTable {
        static TABLE: std::sync::OnceLock<PriceTable> = std::sync::OnceLock::new();
        TABLE.get_or_init(|| {
            let mut table: Value = serde_json::from_str(include_str!("prices.json")).unwrap();
            table["models"]["example-decider"] = json!({"billing":"list_price","periods":{"off_peak":{"uncached_input":0.1,"cache_read":0.1,"cache_write":0.1,"output":0.0}}});
            PriceTable::from_json(&table.to_string()).unwrap()
        })
    }

    #[test]
    fn scripted_episode_memory_is_valid_prose_not_the_rendered_transcript() {
        let rendered = "[user-1 User]\n[source user-1: Typed request]\nAdd top_words.\n\n[asst-1 Assistant]\nPatching.\nTool call call-1 (js) arguments (9 tokens):\n{\"code\":\"*** Begin Patch\"}\n\n[call-1 Tool]\n[tool output call-1, 30 chars, ok]\n<｜｜DSML｜｜ calls>\nTool call ID: call-1";
        let memory = scripted_episode_memory(rendered, 800);
        assert_eq!(
            crate::episode_models::summary_output_reason("summary", &memory),
            None,
            "{memory}"
        );
        assert!(memory.contains("covered 3 messages"));
        assert!(memory.contains("The user wrote (source user-1): Add top_words."));
        assert!(memory.contains("The agent wrote (source asst-1): Patching."));
        assert!(memory.contains("A tool returned (source call-1): (no plain text)"));
        assert!(!memory.contains("Tool call "));
        let clipped = scripted_episode_memory(rendered, 12);
        assert_eq!(
            crate::episode_models::summary_output_reason("summary", &clipped),
            None
        );
    }

    fn gap_corpus(messages: Vec<Message>) -> ReplayCorpus {
        ReplayCorpus {
            chat_id: "archive-gap-fixture".into(),
            recorded_model: None,
            context_window: None,
            instructions: String::new(),
            messages,
            message_metadata: json!([]),
            boundary_gold: None,
            relates_gold: vec![],
            provenance: json!({"fixture":true}),
        }
    }
    fn archive_call(id: &str) -> bubblepaw_core::ToolCall {
        bubblepaw_core::ToolCall {
            id: id.into(),
            name: "must_never_execute".into(),
            arguments: json!({}),
        }
    }

    #[test]
    fn archive_unknown_outcomes_preserve_parallel_results_and_are_idempotent() -> Result<()> {
        let mut calls = performance_message(1, Role::Assistant);
        calls.tool_calls = vec![
            archive_call("missing-a"),
            archive_call("recorded-b"),
            archive_call("missing-c"),
        ];
        let mut result = performance_message(2, Role::Tool);
        result.tool_call_id = Some("recorded-b".into());
        result.content = "Actual recorded result".into();
        let mut corpus = gap_corpus(vec![
            performance_message(0, Role::User),
            calls,
            result,
            performance_message(3, Role::Assistant),
            performance_message(4, Role::User),
        ]);
        let original = corpus.clone();
        let info = normalize_archive(&mut corpus)?;
        assert_eq!(info.source_messages_total, 5);
        assert_eq!(info.marker_steps, vec![4, 5]);
        assert_eq!(
            info.source_corpus_sha256,
            crate::probes::fingerprint(&original)?
        );
        assert_eq!(
            corpus.messages[3].tool_call_id.as_deref(),
            Some("missing-a")
        );
        assert_eq!(
            corpus.messages[4].tool_call_id.as_deref(),
            Some("missing-c")
        );
        for marker in &corpus.messages[3..5] {
            assert_eq!(marker.role, Role::Tool);
            assert_eq!(marker.origin, Some(MessageOrigin::ArchiveUnknownOutcome));
            assert_eq!(marker.content, ARCHIVE_UNKNOWN_OUTCOME);
            assert!(!marker.is_error);
            assert_eq!(marker.created_at, original.messages[1].created_at);
        }
        let retained: Vec<_> = corpus
            .messages
            .iter()
            .filter(|message| message.origin != Some(MessageOrigin::ArchiveUnknownOutcome))
            .cloned()
            .collect();
        assert_eq!(
            serde_json::to_value(&retained)?,
            serde_json::to_value(&original.messages)?
        );
        let once = serde_json::to_value(&corpus)?;
        let again = normalize_archive(&mut corpus)?;
        assert_eq!(once, serde_json::to_value(&corpus)?);
        assert_eq!(info.replay_corpus_sha256, again.replay_corpus_sha256);
        assert_eq!(info.source_corpus_sha256, again.source_corpus_sha256);
        assert_eq!(info.markers_replayed(3), 0);
        assert_eq!(info.markers_replayed(4), 1);
        corpus.messages[4].content = "invented success".into();
        assert!(normalize_archive(&mut corpus).is_err());
        Ok(())
    }

    #[test]
    fn archive_unknown_outcomes_do_not_replace_late_results_and_reject_ambiguity() -> Result<()> {
        let mut calls = performance_message(0, Role::Assistant);
        calls.tool_calls = vec![archive_call("late"), archive_call("absent")];
        let mut result = performance_message(3, Role::Tool);
        result.tool_call_id = Some("late".into());
        let mut corpus = gap_corpus(vec![
            calls.clone(),
            performance_message(1, Role::Assistant),
            performance_message(2, Role::User),
            result,
        ]);
        let info = normalize_archive(&mut corpus)?;
        assert_eq!(info.marker_steps, vec![2]);
        assert_eq!(corpus.messages[1].tool_call_id.as_deref(), Some("absent"));
        assert_eq!(
            corpus.messages.last().unwrap().tool_call_id.as_deref(),
            Some("late")
        );
        let mut eof = gap_corpus(vec![calls.clone()]);
        assert_eq!(normalize_archive(&mut eof)?.marker_steps, vec![2, 3]);
        let mut next_round = performance_message(1, Role::Assistant);
        next_round.tool_calls = vec![archive_call("next")];
        let mut rounds = gap_corpus(vec![calls.clone(), next_round]);
        assert_eq!(normalize_archive(&mut rounds)?.marker_steps, vec![2, 3, 5]);
        let mut duplicate = gap_corpus(vec![calls.clone(), calls.clone()]);
        assert!(normalize_archive(&mut duplicate).is_err());
        let mut duplicate_calls = calls.clone();
        duplicate_calls.tool_calls.push(archive_call("late"));
        assert!(normalize_archive(&mut gap_corpus(vec![duplicate_calls])).is_err());
        let mut duplicate_result = performance_message(1, Role::Tool);
        duplicate_result.tool_call_id = Some("late".into());
        let mut result_again = duplicate_result.clone();
        result_again.id = "different-message".into();
        assert!(
            normalize_archive(&mut gap_corpus(vec![calls, duplicate_result, result_again]))
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn archive_unknown_outcomes_pin_identical_planner_and_answering_positions() -> Result<()>
    {
        let directory = tempfile::tempdir()?;
        let mut first = performance_message(1, Role::Assistant);
        first.content = "transient ".repeat(36_000);
        first.tool_calls = vec![archive_call("first-gap")];
        let mut last = performance_message(3, Role::Assistant);
        last.content = "transient ".repeat(36_000);
        last.tool_calls = vec![archive_call("eof-gap")];
        let corpus = gap_corpus(vec![
            performance_message(0, Role::User),
            first,
            performance_message(2, Role::User),
            last,
        ]);
        let source_sha = crate::probes::fingerprint(&corpus)?;
        let probes = directory.path().join("probes.json");
        std::fs::write(
            &probes,
            serde_json::to_vec(
                &json!([{"id":"p1","type":"recall","question":"What was requested?","expected_answer":"fixture","evidence_message_ids":[corpus.messages[0].id],"earliest_checkpoint":1,"ask_at_turn":2}]),
            )?,
        )?;
        let mut options = ReplayOptions {
            prices: example_prices(),
            agents: Default::default(),
            chat: "fixture".into(),
            out: directory.path().join("planning"),
            threshold: Some(80_000),
            dry_run: true,
            context_strategy: ContextStrategy::Lcm,
            probes: Some(probes),
            checkpoints_at_turns: vec![1, 2],
            probe_plan: None,
            plan_probes: true,
            capture_turn_frontiers: true,
            probe_limit: None,
            max_cost_usd: None,
        };
        let plan_report = build_probe_plan(
            corpus.clone(),
            options.clone(),
            EpisodeModelConfig::default(),
            ProviderEndpoints::default(),
            default_replay_summary_config(),
        )
        .await?;
        assert_eq!(plan_report["corpus_sha256"], source_sha);
        assert_eq!(plan_report["archive_unknown_outcome_markers_total"], 2);
        assert_eq!(plan_report["plan"]["checkpoints"][0]["step"], 3);
        assert_eq!(plan_report["plan"]["checkpoints"][1]["step"], 6);
        let plan_path = options.out.join("probe-plan.json");
        options.probe_plan = Some(plan_path);
        options.plan_probes = false;
        options.probes = None;
        options.checkpoints_at_turns.clear();
        for strategy in [ContextStrategy::Lcm, ContextStrategy::Classical] {
            options.context_strategy = strategy;
            options.out = directory.path().join(format!("answer-{strategy:?}"));
            let report = run_corpus(corpus.clone(), options.clone()).await?;
            assert_eq!(report["status"], "complete");
            assert_eq!(report["source_messages_replayed"], 4);
            assert_eq!(report["messages_replayed"], 6);
            assert_eq!(report["archive_unknown_outcome_markers"], 2);
            assert!(
                report["probes"]["plan_mismatches"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                report["archive_normalization"],
                plan_report["archive_normalization"]
            );
            let store = Store::open_data_dir(options.out.join("store"))?;
            let session = report["session_id"].as_str().unwrap();
            assert!(store.pending_effects(session)?.is_empty());
            let archived = store.messages(session)?;
            assert_eq!(
                archived
                    .iter()
                    .filter(|message| message.origin == Some(MessageOrigin::ArchiveUnknownOutcome))
                    .count(),
                2
            );
            assert_eq!(archived[2].content, ARCHIVE_UNKNOWN_OUTCOME);
            let progress: Value =
                serde_json::from_slice(&std::fs::read(options.out.join("progress.json"))?)?;
            assert_eq!(progress["source_messages_replayed"], 4);
            assert_eq!(progress["archive_unknown_outcome_markers"], 2);
        }
        let mut changed = corpus;
        changed.messages[0].content.push_str("changed");
        options.out = directory.path().join("reject-changed-source");
        assert!(run_corpus(changed, options).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn replay_waits_for_a_complete_episode_before_accepting_a_subject_transition()
    -> Result<()> {
        let output = tempfile::tempdir()?;
        let make_message = |index: usize, role: Role, content: String| Message {
            id: format!("r{index}"),
            session_id: "fixture".into(),
            origin: (role == Role::User).then_some(MessageOrigin::Typed),
            role,
            content,
            tool_calls: vec![],
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: "2026-10-08T00:00:00Z".into(),
        };
        let mut corpus = ReplayCorpus {
            chat_id: "pending-boundary-fixture".into(),
            recorded_model: None,
            context_window: None,
            instructions: "Fixture instructions".into(),
            messages: vec![
                make_message(1, Role::User, "Please work on the old subject.".into()),
                make_message(2, Role::Assistant, "transient ".repeat(20_000)),
                make_message(
                    3,
                    Role::User,
                    "[new subject] Please start the new task.".into(),
                ),
                make_message(4, Role::Assistant, "transient ".repeat(15_000)),
                make_message(5, Role::User, "Continue the new task.".into()),
                make_message(6, Role::Assistant, "The new task continues.".into()),
            ],
            message_metadata: json!([]),
            boundary_gold: Some(
                json!({"source":"fixture","gold":["r3"],"ambiguous":[],"chapters":[]}),
            ),
            relates_gold: vec![],
            provenance: json!({"fixture":true}),
        };
        let options = ReplayOptions {
            prices: example_prices(),
            agents: Default::default(),
            chat: "fixture".into(),
            out: output.path().into(),
            threshold: Some(60_000),
            dry_run: true,
            context_strategy: ContextStrategy::Lcm,
            probes: None,
            checkpoints_at_turns: vec![],
            probe_plan: None,
            plan_probes: false,
            capture_turn_frontiers: false,
            probe_limit: None,
            max_cost_usd: None,
        };
        let report = run_corpus(corpus.clone(), options.clone()).await?;
        assert_eq!(report["status"], "complete");
        assert_eq!(report["episodes"].as_array().unwrap().len(), 1);
        assert_eq!(report["chapters"].as_array().unwrap().len(), 1);
        assert_eq!(report["chapters"][0]["status"], "open");
        assert_eq!(report["episodes"][0]["episode"]["first_message_id"], "r1");
        assert_eq!(report["episodes"][0]["episode"]["last_message_id"], "r2");
        assert_eq!(report["decisions"], json!([]));
        assert_eq!(report["boundary_scores"]["predicted"], json!([]));
        assert_eq!(report["boundary_scores"]["exact"]["fn"], 1);
        assert_eq!(report["messages_replayed"], 6);

        // The short new subject spans two typed turns. It becomes eligible only
        // after another complete round reaches the source target; its interior
        // continuation request r5 never becomes a chapter boundary.
        corpus.messages.push(make_message(
            7,
            Role::Assistant,
            "transient ".repeat(10_000),
        ));
        let mut options = options;
        options.out = output.path().join("completed-episode");
        let report = run_corpus(corpus.clone(), options.clone()).await?;
        assert_eq!(report["status"], "complete");
        assert_eq!(report["episodes"].as_array().unwrap().len(), 2);
        assert_eq!(report["episodes"][1]["episode"]["first_message_id"], "r3");
        assert_eq!(report["episodes"][1]["episode"]["last_message_id"], "r7");
        assert_eq!(report["chapters"].as_array().unwrap().len(), 2);
        assert_eq!(report["chapters"][0]["status"], "closed");
        assert_eq!(report["chapters"][1]["status"], "open");
        assert_eq!(report["decisions"][0]["choice"], "r3");
        assert_eq!(report["decisions"][0]["option_message_ids"], json!(["r3"]));
        assert_eq!(report["boundary_scores"]["predicted"], json!(["r3"]));
        assert_eq!(
            report["boundary_scores"]["materialized_boundaries"],
            json!(["r3"])
        );
        assert_eq!(
            report["boundary_scores"]["unmaterialized_boundaries"],
            json!([])
        );
        assert_eq!(report["boundary_scores"]["exact"]["f1"], 1.0);
        assert_eq!(report["boundary_scores"]["tolerant"]["f1"], 1.0);
        assert_eq!(report["messages_replayed"], 7);

        // A later phase can begin at an assistant episode, with no new human
        // input. Keep its marker beyond the old 1,000-character fake preview.
        corpus.messages.push(make_message(
            8,
            Role::Assistant,
            format!(
                "{}[new subject] Begin validation. {}",
                "preface ".repeat(250),
                "transient ".repeat(25_000)
            ),
        ));
        corpus.boundary_gold.as_mut().unwrap()["gold"] = json!(["r3", "r8"]);
        options.out = output.path().join("agent-phase");
        let report = run_corpus(corpus, options).await?;
        assert_eq!(report["status"], "complete");
        assert_eq!(report["messages_replayed"], 8);
        assert_eq!(report["episodes"].as_array().unwrap().len(), 3);
        assert_eq!(report["chapters"].as_array().unwrap().len(), 3);
        assert_eq!(report["decisions"][1]["choice"], "r8");
        assert_eq!(report["boundary_scores"]["predicted"], json!(["r3", "r8"]));
        assert_eq!(
            report["boundary_scores"]["materialized_boundaries"],
            json!(["r3", "r8"])
        );
        assert_eq!(
            report["boundary_scores"]["unmaterialized_boundaries"],
            json!([])
        );
        assert_eq!(report["boundary_scores"]["exact"]["f1"], 1.0);
        for episode in report["episodes"].as_array().unwrap() {
            assert!(
                episode["summary_tokens"].as_u64().unwrap()
                    <= episode["episode"]["cap"].as_u64().unwrap()
            );
        }
        let calls: Vec<_> = report["calls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|call| call["kind"].as_str().unwrap())
            .collect();
        assert_eq!(
            calls,
            [
                "summary", "summary", "decision", "chapter", "summary", "decision", "chapter"
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn progress_json_is_metadata_only_and_matches_report_counts() -> Result<()> {
        let output = tempfile::tempdir()?;
        let make_message = |index: usize, role: Role, content: String| Message {
            id: format!("p{index}"),
            session_id: "fixture".into(),
            origin: (role == Role::User).then_some(MessageOrigin::Typed),
            role,
            content,
            tool_calls: vec![],
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: "2026-10-09T00:00:00Z".into(),
        };
        let corpus = ReplayCorpus {
            chat_id: "progress-fixture".into(),
            recorded_model: None,
            context_window: None,
            instructions: "Fixture instructions".into(),
            messages: vec![
                make_message(1, Role::User, "PRIVATE_USER_SENTENCE alpha.".into()),
                make_message(2, Role::Assistant, "PRIVATE_TRANSCRIPT ".repeat(20_000)),
                make_message(
                    3,
                    Role::User,
                    "[new subject] PRIVATE_USER_SENTENCE beta.".into(),
                ),
                make_message(4, Role::Assistant, "PRIVATE_TRANSCRIPT ".repeat(15_000)),
                make_message(5, Role::User, "PRIVATE_USER_SENTENCE gamma.".into()),
                make_message(6, Role::Assistant, "PRIVATE_TRANSCRIPT done.".into()),
            ],
            message_metadata: json!([]),
            boundary_gold: None,
            relates_gold: vec![],
            provenance: json!({"fixture":true}),
        };
        let report = run_corpus(
            corpus,
            ReplayOptions {
                prices: example_prices(),
                agents: Default::default(),
                chat: "fixture".into(),
                out: output.path().into(),
                threshold: Some(60_000),
                dry_run: true,
                context_strategy: ContextStrategy::Lcm,
                probes: None,
                checkpoints_at_turns: vec![],
                probe_plan: None,
                plan_probes: false,
                capture_turn_frontiers: false,
                probe_limit: None,
                max_cost_usd: None,
            },
        )
        .await?;
        let text = std::fs::read_to_string(output.path().join("progress.json"))?;
        let progress: Value = serde_json::from_str(&text)?;
        assert_eq!(progress["status"], "loop_finished");
        assert_eq!(progress["messages_imported"], 6);
        assert_eq!(progress["messages_total"], 6);
        assert_eq!(
            progress["compactions_completed"],
            report["compaction_count"]
        );
        assert!(report["compaction_count"].as_u64().unwrap() > 0);
        assert_eq!(
            progress["projected_cost"]["summary_and_chapter"]["projected_cost_usd"],
            report["totals"]["summarization"]["projected_paid_cost_usd"]
        );
        assert_eq!(
            progress["projected_cost"]["decision"]["projected_cost_usd"],
            report["totals"]["decision"]["projected_paid_cost_usd"]
        );
        assert_eq!(progress["resolved_threshold"], 60_000);
        assert_eq!(progress["phase_timings"]["current_operation"], Value::Null);
        assert_eq!(
            progress["storage"]["canonical_import_transactions"],
            report["storage_transaction_basis"]["canonical_import_transactions"]
        );
        assert_eq!(
            progress["storage"]["transaction_count"],
            report["storage"]["transaction_count"]
        );
        for phase in [
            "canonical_import",
            "masking_token_estimation",
            "worker_compaction",
        ] {
            assert!(report["phase_timings"]["seconds"][phase].as_f64().unwrap() >= 0.0);
        }
        let store_writes = report["phase_timings"]["store_writes_seconds"]
            .as_f64()
            .unwrap();
        let canonical_writes = report["phase_timings"]["canonical_store_writes_seconds"]
            .as_f64()
            .unwrap();
        let internal_writes = report["phase_timings"]["internal_store_writes_seconds"]
            .as_f64()
            .unwrap();
        assert!(canonical_writes > 0.0);
        assert!((store_writes - canonical_writes - internal_writes).abs() < 1e-9);
        assert!(
            canonical_writes
                <= report["phase_timings"]["seconds"]["canonical_commit"]
                    .as_f64()
                    .unwrap()
        );
        assert!(progress["elapsed_seconds"].as_f64().unwrap() >= 0.0);
        assert!(progress["projected_cost"]["summary_and_chapter"]["calls"].is_u64());
        assert!(progress["projected_cost"]["decision"]["projected_cost_usd"].is_f64());
        assert!(
            !text.contains("PRIVATE_"),
            "progress must hold no transcript text"
        );
        assert!(!output.path().join("progress.json.tmp").exists());
        Ok(())
    }

    #[test]
    fn progress_captures_an_inflight_phase_without_reading_history() -> Result<()> {
        let output = tempfile::tempdir()?;
        let writer = ProgressWriter {
            out: output.path().into(),
            telemetry: Arc::default(),
            started: Instant::now(),
            threshold: 60_000,
            messages_total: 10,
            archive_normalization: ArchiveNormalization {
                source_messages_total: 10,
                ..Default::default()
            },
            store: Store::in_memory()?,
            phases: Arc::default(),
            snapshots: Arc::default(),
        };
        writer.counts(3, 1, 2);
        let phase = writer.phase("masking_token_estimation", 4);
        writer.write("running", 3, 1)?;
        let progress: Value =
            serde_json::from_slice(&std::fs::read(output.path().join("progress.json"))?)?;
        assert_eq!(
            progress["phase_timings"]["current_operation"]["phase"],
            "masking_token_estimation"
        );
        assert_eq!(progress["phase_timings"]["current_operation"]["step"], 4);
        assert!(
            progress["phase_timings"]["current_operation"]["elapsed_seconds"]
                .as_f64()
                .unwrap()
                >= 0.0
        );
        assert_eq!(progress["storage"]["canonical_import_transactions"], 2);
        drop(phase);
        assert_eq!(writer.timings()["current_operation"], Value::Null);
        Ok(())
    }

    fn performance_message(index: usize, role: Role) -> Message {
        Message {
            id: format!("gate-{index}"),
            session_id: "fixture".into(),
            origin: (role == Role::User).then_some(MessageOrigin::Typed),
            role,
            content: format!("Fixture {index}"),
            tool_calls: vec![],
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: "2026-10-10T00:00:00Z".into(),
        }
    }

    #[test]
    fn replay_readiness_matches_prefix_planner_with_multi_turn_episodes_and_parallel_tools()
    -> Result<()> {
        let mut messages = vec![
            performance_message(0, Role::User),
            performance_message(1, Role::Assistant),
            performance_message(2, Role::User),
            performance_message(3, Role::Assistant),
            performance_message(4, Role::Tool),
            performance_message(5, Role::Tool),
            performance_message(6, Role::Assistant),
            performance_message(7, Role::User),
            performance_message(8, Role::Assistant),
            performance_message(9, Role::Assistant),
            performance_message(10, Role::User),
            performance_message(11, Role::Assistant),
        ];
        messages[2].content = "[new subject] Change task".into();
        messages[3].tool_calls = vec![
            bubblepaw_core::ToolCall {
                id: "parallel-a".into(),
                name: "recorded_only".into(),
                arguments: json!({}),
            },
            bubblepaw_core::ToolCall {
                id: "parallel-b".into(),
                name: "recorded_only".into(),
                arguments: json!({}),
            },
        ];
        messages[4].tool_call_id = Some("parallel-a".into());
        messages[5].tool_call_id = Some("parallel-b".into());
        // Exact synthetic canonical counts exercise threshold edges without large text.
        let counts = [
            12, 10_000, 12, 4_000, 4_000, 8_000, 12, 12, 70_000, 33_000, 12, 12,
        ];
        let mut gate = ReplayWorkerGate::new();
        let mut pending = HashSet::new();
        let mut coverage = 0;
        let mut drained_ranges = Vec::new();
        let mut ready_steps = Vec::new();
        for (index, (message, tokens)) in messages.iter().zip(counts).enumerate() {
            gate.observe(message, tokens)?;
            for call in &message.tool_calls {
                pending.insert(call.id.clone());
            }
            if let Some(call) = &message.tool_call_id {
                pending.remove(call);
            }
            let history = &messages[..=index];
            let oracle = crate::episodes::plan_episodes(
                history,
                coverage,
                |message| gate.canonical_tokens[&message.id],
                is_typed_user,
            )?;
            let ready = gate.needs_lcm(history, &pending)?;
            assert_eq!(
                ready,
                !oracle.episodes.is_empty(),
                "step {} coverage {}",
                index + 1,
                coverage
            );
            if ready {
                ready_steps.push(index + 1);
            }
            // Simulate the worker's repeated first-episode drain. Chapter decisions
            // can select only the completed episode's start, leaving coverage intact.
            loop {
                let plan = crate::episodes::plan_episodes(
                    history,
                    coverage,
                    |message| gate.canonical_tokens[&message.id],
                    is_typed_user,
                )?;
                let Some(first) = plan.episodes.into_iter().next() else {
                    break;
                };
                drained_ranges.push((first.message_range.start, first.message_range.end));
                coverage = first.message_range.end;
            }
            gate.coverage_end = coverage;
        }
        assert_eq!(
            drained_ranges[0],
            (0, 6),
            "Both typed turns and parallel results stay in one episode"
        );
        assert!(
            !ready_steps.contains(&4),
            "Pending parallel calls cannot make a size episode ready"
        );
        assert!(
            !ready_steps.contains(&5),
            "One of two parallel results is still pending"
        );
        assert!(
            ready_steps.contains(&6),
            "Completing the oversized parallel round creates an episode"
        );
        Ok(())
    }

    #[test]
    fn replay_readiness_keeps_pending_call_errors_at_first_activation_and_new_typed_input()
    -> Result<()> {
        let mut messages = vec![
            performance_message(0, Role::User),
            performance_message(1, Role::Assistant),
            performance_message(2, Role::User),
        ];
        messages[1].tool_calls.push(bubblepaw_core::ToolCall {
            id: "unfinished".into(),
            name: "recorded_only".into(),
            arguments: json!({}),
        });
        for activate_at_end in [false, true] {
            let mut gate = ReplayWorkerGate::new();
            let mut pending = HashSet::new();
            for (index, message) in messages.iter().enumerate() {
                gate.observe(message, 1)?;
                for call in &message.tool_calls {
                    pending.insert(call.id.clone());
                }
                if index < 2 && !activate_at_end {
                    assert!(!gate.needs_lcm(&messages[..=index], &pending)?);
                }
            }
            let error = gate
                .needs_lcm(&messages, &pending)
                .expect_err("Typed input interrupting a tool round must still fail");
            assert!(error.to_string().contains("incomplete tool round"));
        }
        Ok(())
    }

    #[test]
    fn replay_readiness_uses_the_workers_configured_source_policy() -> Result<()> {
        let config = EpisodeConfig {
            source_target_tokens: 2_000,
            source_max_tokens: 4_000,
            ..EpisodeConfig::default()
        };
        let messages = [
            performance_message(0, Role::User),
            performance_message(1, Role::Assistant),
            performance_message(2, Role::User),
            performance_message(3, Role::Assistant),
        ];
        let mut gate = ReplayWorkerGate::with_config(config.clone());
        let mut readiness = Vec::new();
        for (index, (message, tokens)) in messages.iter().zip([1, 1_500, 1, 1_500]).enumerate() {
            gate.observe(message, tokens)?;
            let history = &messages[..=index];
            let plan = crate::episodes::plan_episodes_with_config(
                history,
                0,
                |message| gate.canonical_tokens[&message.id],
                is_typed_user,
                &config,
            )?;
            let ready = gate.needs_lcm(history, &HashSet::new())?;
            assert_eq!(ready, !plan.episodes.is_empty());
            readiness.push(ready);
        }
        assert_eq!(readiness, [false, false, false, true]);
        Ok(())
    }

    #[test]
    fn replay_import_batch_rolls_back_all_records_and_only_counts_commits() -> Result<()> {
        let mut db = Store::in_memory()?;
        let session = db.create_session("batch rollback")?;
        let mut batch = ReplayImportBatch::default();
        let mut message = performance_message(0, Role::User);
        message.session_id = session.id.clone();
        batch.push(&message)?;
        let mut invalid = message.clone();
        invalid.session_id = "missing-session".into();
        batch.push(&invalid)?;
        assert!(batch.flush(&mut db).is_err());
        assert!(db.messages(&session.id)?.is_empty());
        assert_eq!(batch.committed_messages, 0);
        assert_eq!(batch.committed_transactions, 0);
        assert_eq!(
            batch.records.len(),
            2,
            "Failed batch remains available for diagnostics"
        );
        batch.records.pop();
        batch.flush(&mut db)?;
        assert_eq!(batch.committed_messages, 1);
        assert_eq!(batch.committed_transactions, 1);
        assert!(batch.records.is_empty());
        batch.flush(&mut db)?;
        assert_eq!(
            batch.committed_transactions, 1,
            "Empty flush is not a write transaction"
        );
        Ok(())
    }

    #[tokio::test]
    async fn replay_batch_and_worker_gates_preserve_both_strategy_contracts() -> Result<()> {
        for malformed in [false, true] {
            let mut messages = Vec::new();
            for turn in 0..4 {
                let mut user = performance_message(messages.len(), Role::User);
                user.content = if turn == 1 {
                    "[new subject] Switch to task beta".into()
                } else {
                    format!("Continue task {turn}")
                };
                messages.push(user);
                let mut assistant = performance_message(messages.len(), Role::Assistant);
                assistant.content = "transient ".repeat(36_000);
                if turn == 0 {
                    assistant.tool_calls = vec![
                        bubblepaw_core::ToolCall {
                            id: "fixture-parallel-a".into(),
                            name: "recorded_only".into(),
                            arguments: json!({"path":"a"}),
                        },
                        bubblepaw_core::ToolCall {
                            id: "fixture-parallel-b".into(),
                            name: "recorded_only".into(),
                            arguments: json!({"path":"b"}),
                        },
                    ];
                }
                messages.push(assistant);
                if turn == 0 {
                    for call in ["fixture-parallel-a", "fixture-parallel-b"] {
                        let mut tool = performance_message(messages.len(), Role::Tool);
                        tool.tool_call_id = Some(call.into());
                        tool.content = "Recorded result; never execute this tool. ".repeat(500);
                        messages.push(tool);
                    }
                }
            }
            if malformed {
                let mut assistant = performance_message(messages.len(), Role::Assistant);
                assistant.tool_calls.push(bubblepaw_core::ToolCall {
                    id: "fixture-unresolved".into(),
                    name: "recorded_only".into(),
                    arguments: json!({}),
                });
                messages.push(assistant);
                messages.push(performance_message(messages.len(), Role::User));
            }
            let corpus = ReplayCorpus {
                chat_id: "performance-oracle".into(),
                recorded_model: None,
                context_window: None,
                instructions: "Keep fixture facts".into(),
                messages: messages.clone(),
                message_metadata: json!([]),
                boundary_gold: None,
                relates_gold: vec![],
                provenance: json!({"fixture":true}),
            };
            let mut normalized_corpus = corpus.clone();
            normalize_archive(&mut normalized_corpus)?;
            let messages = normalized_corpus.messages;
            let canonical_ids: HashSet<_> =
                messages.iter().map(|message| message.id.clone()).collect();
            for strategy in [ContextStrategy::Lcm, ContextStrategy::Classical] {
                let mut contracts = Vec::new();
                let mut commits = Vec::new();
                for execution in [ReplayExecution::PerMessage, ReplayExecution::Optimized] {
                    let directory = tempfile::tempdir()?;
                    let options = ReplayOptions {
                        prices: example_prices(),
                        agents: Default::default(),
                        chat: "fixture".into(),
                        out: directory.path().into(),
                        threshold: Some(80_000),
                        dry_run: true,
                        context_strategy: strategy,
                        probes: None,
                        checkpoints_at_turns: vec![1, 2, 3, 4],
                        probe_plan: None,
                        plan_probes: false,
                        capture_turn_frontiers: true,
                        probe_limit: None,
                        max_cost_usd: None,
                    };
                    let prepared_trace = Mutex::new(Vec::new());
                    let mut report = run_corpus_inner(
                        corpus.clone(),
                        options,
                        EpisodeModelConfig::default(),
                        ProviderEndpoints::default(),
                        default_replay_summary_config(),
                        execution,
                        Some(&prepared_trace),
                    )
                    .await?;
                    let store = Store::open_data_dir(directory.path().join("store"))?;
                    let session = report["session_id"].as_str().unwrap().to_owned();
                    let archived = store.messages(&session)?;
                    assert_eq!(archived.len(), messages.len());
                    for (expected, actual) in messages.iter().zip(archived) {
                        let mut actual = actual;
                        actual.session_id = expected.session_id.clone();
                        assert_eq!(
                            serde_json::to_value(actual)?,
                            serde_json::to_value(expected)?,
                            "Canonical history must retain exact IDs, content, tools and original timestamps"
                        );
                    }
                    assert!(store.pending_effects(&session)?.is_empty());
                    report["prepared_requests"] =
                        serde_json::to_value(&*prepared_trace.lock().unwrap())?;
                    assert!(
                        !report["prepared_requests"].as_array().unwrap().is_empty(),
                        "Capture every checkpoint/compaction request, not only the final view"
                    );
                    report["durable_serving_view"] =
                        serde_json::to_value(store.context_view(&session)?)?;
                    commits.push(
                        report["storage_transaction_basis"]["canonical_import_transactions"]
                            .as_u64()
                            .unwrap(),
                    );
                    assert_eq!(report["status"], "complete");
                    assert_eq!(
                        report["archive_unknown_outcome_markers"],
                        usize::from(malformed)
                    );
                    assert_eq!(report["source_messages_replayed"], corpus.messages.len());
                    contracts.push(normalized_replay_contract(
                        report,
                        &canonical_ids,
                        &messages,
                    ));
                }
                assert_eq!(
                    contracts[0], contracts[1],
                    "strategy {strategy:?}, malformed {malformed}"
                );
                assert!(
                    commits[1] < commits[0],
                    "Batching must reduce actual canonical commits"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn replay_archive_id_index_accelerates_the_existing_store_query() -> Result<()> {
        let db = rusqlite::Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE messages(seq INTEGER PRIMARY KEY,session_id TEXT,data TEXT); CREATE INDEX messages_session ON messages(session_id,seq); CREATE INDEX replay_message_id ON messages(session_id,json_extract(data,'$.id'));")?;
        let plan = db.query_row("EXPLAIN QUERY PLAN SELECT seq FROM messages WHERE session_id=?1 AND json_extract(data,'$.id')=?2",
            ["fixture", "gate-0"], |row| row.get::<_, String>(3))?;
        assert!(
            plan.contains("replay_message_id"),
            "ID lookup must use the expression index: {plan}"
        );
        Ok(())
    }

    #[test]
    fn checkpoint_guard_rejects_frontier_drift_with_covered_or_empty_common_lists() -> Result<()> {
        let probes: Vec<crate::probes::Probe> = serde_json::from_value(json!([
            {"id":"p1","type":"recall","question":"What colour?","expected_answer":"blue","evidence_message_ids":["r1"],"earliest_checkpoint":1,"ask_at_turn":1},
            {"id":"p2","type":"recall","question":"Which colour?","expected_answer":"blue","evidence_message_ids":["r1"],"earliest_checkpoint":1,"ask_at_turn":1}
        ]))?;
        let history: Vec<Message> = ["r1", "r2", "r3"]
            .into_iter()
            .map(|id| Message {
                id: id.into(),
                session_id: "fixture".into(),
                role: Role::Assistant,
                origin: None,
                content: String::new(),
                tool_calls: vec![],
                tool_call_id: None,
                provider_state: None,
                is_error: false,
                created_at: String::new(),
            })
            .collect();
        let mut planned = crate::probes::PlanCheckpoint {
            checkpoint: 1,
            turn: 1,
            step: 3,
            end_message_id: "r3".into(),
            common_probe_ids: vec!["p1".into(), "p2".into()],
            skipped: vec![],
            arm_frontiers: BTreeMap::from([
                ("lcm".into(), json!({"frontier_message_id":"r2"})),
                ("classical".into(), json!({"frontier_message_id":"r2"})),
            ]),
        };
        for arm in crate::probes::STRATEGIES {
            assert!(!checkpoint_mismatch(
                &planned,
                arm,
                &probes,
                &history,
                Some("r2")
            ));
            for actual in ["r1", "r3"] {
                assert!(probes.iter().all(|probe| crate::probes::eligible(
                    probe,
                    1,
                    1,
                    &history,
                    Some(actual)
                )));
                // Reject the whole checkpoint even though every selected probe
                // still has its evidence behind the changed actual frontier.
                assert!(checkpoint_mismatch(
                    &planned,
                    arm,
                    &probes,
                    &history,
                    Some(actual)
                ));
            }
        }
        planned.common_probe_ids.clear();
        for arm in crate::probes::STRATEGIES {
            assert!(checkpoint_mismatch(
                &planned,
                arm,
                &probes,
                &history,
                Some("r1")
            ));
            assert!(checkpoint_mismatch(&planned, arm, &probes, &history, None));
            assert!(!checkpoint_mismatch(
                &planned,
                arm,
                &probes,
                &history,
                Some("r2")
            ));
        }
        Ok(())
    }
    #[tokio::test]
    async fn common_plan_consumers_keep_identical_lists_and_record_frontier_drift_without_calls()
    -> Result<()> {
        let directory = tempfile::tempdir()?;
        let make = |index: usize, role: Role| Message {
            id: format!("r{index}"),
            session_id: "original".into(),
            role,
            origin: None,
            content: "Garden blue".into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: String::new(),
        };
        let corpus = ReplayCorpus {
            chat_id: "plan-fixture".into(),
            recorded_model: None,
            context_window: None,
            instructions: String::new(),
            messages: vec![
                make(1, Role::User),
                make(2, Role::Assistant),
                make(3, Role::User),
                make(4, Role::Assistant),
            ],
            message_metadata: Value::Null,
            boundary_gold: None,
            relates_gold: vec![],
            provenance: Value::Null,
        };
        let probe = crate::probes::Probe {
            id: "p1".into(),
            kind: crate::probes::ProbeType::Recall,
            question: "What colour?".into(),
            expected_answer: "blue".into(),
            evidence_message_ids: vec!["r1".into()],
            earliest_checkpoint: 1,
            ask_at_turn: None,
            origin: None,
            original_question: None,
            source_message_id: None,
            source_turn: None,
        };
        let mut frontiers = BTreeMap::new();
        for strategy in crate::probes::STRATEGIES {
            frontiers.insert(strategy.into(),vec![json!({"turn":1,"step":2,"end_message_id":"r2","frontier_message_id":"r1","compaction_count":3,"status":"complete"})]);
        }
        let config = EpisodeModelConfig::default();
        let endpoints = ProviderEndpoints::default();
        let summary = default_replay_summary_config();
        let plan = crate::probes::ProbePlan {
            schema_version: 1,
            corpus_sha256: crate::probes::fingerprint(&(ARCHIVE_NORMALIZATION_POLICY, &corpus))?,
            probes_sha256: crate::probes::fingerprint(&vec![probe.clone()])?,
            threshold: 50_000,
            checkpoints_at_turns: vec![1],
            model_signature: model_signature(&config, &endpoints, &summary, 50_000),
            strategy_signature: crate::probes::strategy_signature(),
            probe_limit_per_checkpoint: None,
            checkpoints: crate::probes::common_checkpoints(
                std::slice::from_ref(&probe),
                &corpus.messages,
                &[1],
                &frontiers,
                None,
            )?,
            probes: vec![probe],
            all_turn_frontiers: frontiers,
            source_plan_sha256: None,
        };
        let path = directory.path().join("probe-plan.json");
        crate::probes::save_plan(&path, &plan)?;
        for strategy in [ContextStrategy::Lcm, ContextStrategy::Classical] {
            let out = directory
                .path()
                .join(serde_json::to_value(strategy)?.as_str().unwrap());
            let options = ReplayOptions {
                prices: example_prices(),
                agents: Default::default(),
                chat: "fixture".into(),
                out: out.clone(),
                threshold: Some(50_000),
                dry_run: true,
                context_strategy: strategy,
                probes: None,
                checkpoints_at_turns: vec![],
                probe_plan: Some(path.clone()),
                plan_probes: false,
                capture_turn_frontiers: false,
                probe_limit: None,
                max_cost_usd: None,
            };
            let report = run_corpus(corpus.clone(), options.clone()).await?;
            assert_eq!(report["status"], "partial");
            assert_eq!(report["partial"], true);
            assert_eq!(
                report["probe_plan"]["plan"]["checkpoints"][0]["common_probe_ids"],
                json!(["p1"])
            );
            assert_eq!(
                report["probes"]["rows"][0]["skip_reason"],
                "plan_frontier_mismatch"
            );
            assert_eq!(report["totals"]["probe"]["calls"], 0);
            assert_eq!(
                report["turn_frontiers"][0]["comparison_plan"]["actual_compaction_count"],
                0
            );
            assert_eq!(
                report["probes"]["plan_mismatches"][0]["covariates"]["actual_eligible_common_probe_ids"],
                json!([])
            );
            let store = Store::open_data_dir(out.join("store"))?;
            assert_eq!(
                store
                    .messages(report["session_id"].as_str().unwrap())?
                    .len(),
                4
            );
            let mut empty_plan = plan.clone();
            empty_plan.probes[0].earliest_checkpoint = 2;
            empty_plan.probes_sha256 = crate::probes::fingerprint(&empty_plan.probes)?;
            empty_plan.checkpoints = crate::probes::common_checkpoints(
                &empty_plan.probes,
                &corpus.messages,
                &[1],
                &empty_plan.all_turn_frontiers,
                None,
            )?;
            assert!(empty_plan.checkpoints[0].common_probe_ids.is_empty());
            let empty_path = directory.path().join("empty-probe-plan.json");
            crate::probes::save_plan(&empty_path, &empty_plan)?;
            let mut empty_options = options.clone();
            empty_options.out = directory.path().join(format!(
                "empty-{}",
                serde_json::to_value(strategy)?.as_str().unwrap()
            ));
            empty_options.probe_plan = Some(empty_path);
            let empty_report = run_corpus(corpus.clone(), empty_options).await?;
            assert_eq!(empty_report["partial"], true);
            assert_eq!(empty_report["totals"]["probe"]["calls"], 0);
            assert_eq!(
                empty_report["probes"]["plan_mismatches"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
            assert_eq!(
                empty_report["probes"]["plan_mismatches"][0]["covariates"]["actual_eligible_common_probe_ids"],
                json!([])
            );
            let mut bad = options;
            bad.threshold = Some(60_000);
            bad.out = directory.path().join("reject-config");
            assert!(run_corpus(corpus.clone(), bad).await.is_err());
        }
        let mut corrupted: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        corrupted["plan"]["checkpoints"][0]["common_probe_ids"] = json!([]);
        std::fs::write(&path, serde_json::to_vec(&corrupted)?)?;
        assert!(crate::probes::load_plan(&path).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn probe_registry_preserves_worker_namespace_cursor_and_closes_its_mirror() -> Result<()>
    {
        struct Witness {
            requests: Mutex<Vec<ModelRequest>>,
            closed: Mutex<Vec<String>>,
        }
        #[async_trait]
        impl Provider for Witness {
            async fn generate(
                &self,
                request: ModelRequest,
                _: bubblepaw_core::TextCallback,
                _: CancellationToken,
            ) -> Result<bubblepaw_core::ModelResponse> {
                self.requests.lock().unwrap().push(request);
                Ok(bubblepaw_core::ModelResponse {
                    text: "Blue.".into(),
                    provider_state: Some(
                        json!({"adapter":"chat-completions","reasoning_content":"opaque worker reasoning"}),
                    ),
                    ..Default::default()
                })
            }
            async fn close_session(&self, session_id: &str) -> Result<()> {
                self.closed.lock().unwrap().push(session_id.into());
                Ok(())
            }
        }
        let witness = Arc::new(Witness {
            requests: Mutex::new(Vec::new()),
            closed: Mutex::new(Vec::new()),
        });
        let registry = Arc::new(ProviderRegistry::new("opencode", witness.clone()));
        let model = Arc::new(ReplayProbeModel {
            prices: example_prices(),
            context_route: Default::default(),
            telemetry: Arc::new(Mutex::new(Telemetry::default())),
            url: None,
            client: reqwest::Client::new(),
            model: FLASH_MODEL.into(),
            effort: ReasoningEffort::High,
            registry: Some(registry),
        });
        let store = Store::in_memory()?;
        let source = store.create_session("probe source")?;
        let probe: crate::probes::Probe = serde_json::from_value(
            json!({"id":"p","type":"recall","question":"What colour?","expected_answer":"Blue.","evidence_message_ids":[],"earliest_checkpoint":1}),
        )?;
        let answer =
            crate::probes::answer(model, &store, &source.id, &ModelRequest::default(), &probe)
                .await;
        assert_eq!(answer["status"], "answered");
        let child = answer.worker_session_id().unwrap().to_owned();
        {
            let requests = witness.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].session_id, child);
            assert!(!requests[0].history.epoch.is_empty());
            assert_eq!(requests[0].reasoning_effort, Some(ReasoningEffort::High));
            assert_eq!(
                requests[0].messages.last().unwrap().origin,
                Some(MessageOrigin::ChildTask)
            );
        }
        assert_eq!(
            witness.closed.lock().unwrap().as_slice(),
            std::slice::from_ref(&child)
        );
        assert_eq!(
            store
                .messages(&child)?
                .last()
                .unwrap()
                .provider_state
                .as_ref()
                .unwrap()["reasoning_content"],
            "opaque worker reasoning"
        );
        answer.acknowledge().await?;
        assert_eq!(
            witness.closed.lock().unwrap().as_slice(),
            std::slice::from_ref(&child)
        );
        Ok(())
    }

    #[tokio::test]
    async fn scripted_probe_reads_history_even_when_frozen_view_has_recorded_tool_results()
    -> Result<()> {
        let store = Store::in_memory()?;
        let session = store.create_session("test")?;
        let probe = crate::probes::Probe {
            id: "p1".into(),
            kind: crate::probes::ProbeType::Recall,
            question: "What happened?".into(),
            expected_answer: "It worked".into(),
            evidence_message_ids: vec!["r1".into()],
            earliest_checkpoint: 1,
            ask_at_turn: None,
            origin: None,
            original_question: None,
            source_message_id: None,
            source_turn: None,
        };
        let mut frozen = ModelRequest::default();
        frozen.messages.push(Message {
            id: "r1".into(),
            session_id: session.id.clone(),
            role: Role::Tool,
            content: "Original recorded tool output".into(),
            tool_call_id: Some("recorded-tool".into()),
            tool_calls: vec![],
            origin: None,
            provider_state: None,
            is_error: false,
            created_at: String::new(),
        });
        let model = Arc::new(ReplayProbeModel {
            prices: example_prices(),
            context_route: Default::default(),
            telemetry: Arc::new(Mutex::new(Telemetry::default())),
            url: None,
            client: reqwest::Client::new(),
            model: FLASH_MODEL.into(),
            effort: ReasoningEffort::Low,
            registry: None,
        });
        let result =
            crate::probes::answer(model.clone(), &store, &session.id, &frozen, &probe).await;
        assert_eq!(result["status"], "answered");
        assert_eq!(result["calls"].as_array().unwrap().len(), 2);
        assert_eq!(result["tool_calls"].as_array().unwrap().len(), 1);
        assert_eq!(result["tool_calls"][0]["name"], "history_search");
        Ok(())
    }

    #[tokio::test]
    async fn registry_calls_preserve_replay_attempt_usage_and_cost_metering() -> Result<()> {
        use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
        use std::collections::VecDeque;
        type Replies = Arc<Mutex<VecDeque<(StatusCode, Value)>>>;
        async fn respond(
            State(replies): State<Replies>,
            Json(_body): Json<Value>,
        ) -> (StatusCode, Json<Value>) {
            let (status, payload) = replies.lock().unwrap().pop_front().unwrap();
            (status, Json(payload))
        }
        let replies: Replies = Arc::new(Mutex::new(VecDeque::from([
            (StatusCode::BAD_GATEWAY, json!({})),
            (
                StatusCode::OK,
                json!({"choices":[{"message":{"content":"# Summary\nFacts"}}],"usage":{"prompt_tokens":17,"completion_tokens":4,"completion_tokens_details":{"reasoning_tokens":2}}}),
            ),
            (
                StatusCode::OK,
                json!({"model":"example/decider-model","answers":{"choice":{"type":"choice","choice":"none","probabilities":{"1":0.0,"none":1.0}}},"usage":{"input_tokens":11,"output_tokens":2,"cost":0.001}}),
            ),
            (
                StatusCode::OK,
                json!({"model":"example/decider-model","answers":{"choice":{"type":"choice","choice":"none","probabilities":{"1":0.0,"none":1.0}}},"usage":{"input_tokens":11,"output_tokens":2,"cost":0.001}}),
            ),
            (
                StatusCode::OK,
                json!({"model":"example/decider-model","answers":{"choice":{"type":"choice","choice":"none","probabilities":{"1":0.0,"none":1.0}}},"usage":{"input_tokens":11,"output_tokens":2,"cost":0.001}}),
            ),
        ])));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/chat/completions", post(respond))
                    .route("/decision", post(respond))
                    .with_state(replies),
            )
            .await
            .unwrap();
        });
        let directory = tempfile::tempdir()?;
        let ledger = directory.path().join("calls.jsonl");
        let meter = Arc::new(Mutex::new(Telemetry::default()));
        let mut config = config_for_decider("openrouter/example/decider-model");
        let mut endpoints = ProviderEndpoints {
            opencode_base_url: Some(base.clone()),
            openrouter_decisions_url: format!("{base}/decision"),
            ..Default::default()
        };
        let mut keys = ProviderKeys {
            opencode: Some("fake-summary".into()),
            openrouter: Some("fake-decision".into()),
            openai: None,
        };
        let proxy = relay_configured_keys(
            &mut config,
            &mut endpoints,
            meter.clone(),
            ledger.clone(),
            example_prices(),
            &mut keys,
        )
        .await?;
        let mut registry =
            ProviderRegistry::new("replay", Arc::new(ScriptedProvider::new("unused")));
        endpoints.register_with_keys(&mut registry, keys)?;
        let models = HttpEpisodeModels::from_registry(config, Arc::new(registry), None)?;
        meter.lock().unwrap().begin_operation("summary");
        let summary = models
            .summarize(SummaryInput {
                focus: None,
                session_id: "local".into(),
                previous_summary: None,
                turn_request: None,
                episode_text: "facts".into(),
                cap: 100,
            })
            .await?;
        assert_eq!(summary.usage.input_tokens, 17);
        meter.lock().unwrap().begin_operation("decision");
        let decision = models
            .decide(SplitInput {
                session_id: "local".into(),
                open_chapter: "facts".into(),
                options: vec![crate::episode_models::SplitOption {
                    message_id: "u1".into(),
                    user: "continue".into(),
                    agent_before: String::new(),
                }],
            })
            .await?;
        assert_eq!(decision.choice, None);
        assert_eq!(decision.usage.input_tokens, 33);
        {
            let meter = meter.lock().unwrap();
            assert_eq!(meter.calls.len(), 5);
            assert_eq!(meter.retries, 1);
            assert_eq!(meter.failures, 1);
            assert_eq!(meter.calls[0]["outcome"], "error");
            assert_eq!(meter.calls[1]["transport_retry"], true);
            assert_eq!(meter.calls[1]["reasoning_tokens"], 2);
            assert_eq!(meter.calls[4]["request_slot"], 3);
            assert!((meter.flash_cost_usd - (17.0 * 0.0000003 + 4.0 * 0.0000012)).abs() < 1e-12);
            assert!((meter.jev_cost_usd - 0.003).abs() < 1e-12);
            assert!(meter.reserved_unknown_cost_usd > 0.0);
        }
        assert_eq!(std::fs::read_to_string(ledger)?.lines().count(), 5);
        proxy.abort();
        server.abort();
        Ok(())
    }
    fn config_for_decider(model: &str) -> EpisodeModelConfig {
        let mut config = EpisodeModelConfig {
            retry_base_delay: Duration::ZERO,
            ..Default::default()
        };
        config.roles.decider.models = vec![model.into()];
        config
    }

    #[test]
    fn unpriced_replay_models_are_rejected_before_relay_dispatch() {
        let mut unknown_summary = EpisodeModelConfig::default();
        unknown_summary.roles.summary.models = vec!["custom/other-model".into()];
        for config in [
            unknown_summary,
            config_for_decider("openai/unknown-model"),
            config_for_decider("llm/custom/model"),
        ] {
            assert!(replay_roles(&config, example_prices(), true).is_err());
        }
    }

    #[test]
    fn haiku_prices_whole_prompt_threshold_and_cache_subsets() {
        let prices = example_prices();
        let mut usage = Usage {
            input_tokens: 100_000,
            output_tokens: 2_000,
            cache_read_tokens: 60_000,
            cache_creation_tokens: 20_000,
            reasoning_tokens: None,
        };
        let cost = |usage: &Usage| prices.usage_cost(HAIKU_MODEL, usage, None);
        assert!((cost(&usage).unwrap() - 0.0061).abs() < 1e-12);
        usage.input_tokens += 1;
        assert!((cost(&usage).unwrap() - 0.0305005).abs() < 1e-12);
        assert!(
            prices
                .reservation(HAIKU_MODEL, usage.input_tokens, usage.output_tokens)
                .unwrap()
                > cost(&usage).unwrap()
        );
        usage.cache_creation_tokens = 100_000;
        assert!(cost(&usage).is_none());
        // Model names match exactly after the provider prefix, so these are unpriced.
        let small = Usage {
            input_tokens: 1,
            output_tokens: 1,
            ..Default::default()
        };
        assert!(
            prices
                .usage_cost("opencode-messages/claude-haiku-5-5-extra", &small, None)
                .is_none()
        );
        assert!(prices.is_priced(&format!("llm/{HAIKU_MODEL}")));
        let mut config = config_for_decider(&format!("llm/{HAIKU_MODEL}"));
        config.roles.summary.models = vec![HAIKU_MODEL.into(), FLASH_MODEL.into()];
        config.roles.chapter.models = vec![HAIKU_MODEL.into()];
        assert!(replay_roles(&config, prices, true).is_ok());
        config
            .roles
            .summary
            .models
            .push("custom/unpriced-fallback".into());
        assert!(replay_roles(&config, prices, true).is_err());
        // Summary and chapter roles accept only the exact configured ids.
        config.roles.summary.models = vec!["opencode/claude-haiku-5-5".into()];
        assert!(replay_roles(&config, prices, true).is_err());
        let mut meter = Telemetry {
            max_cost_usd: 0.10,
            haiku_cost_usd: 0.04,
            reserved_unknown_cost_usd: 0.02,
            ..Default::default()
        };
        assert!(meter.reserve_next(0.03));
        assert!(!meter.reserve_next(prices.reservation(HAIKU_MODEL, 100_001, 65_536).unwrap()));
    }

    #[tokio::test]
    async fn selected_haiku_roles_probes_and_judge_use_messages_and_cache_aware_metering()
    -> Result<()> {
        use crate::probes::ProbeModel;
        use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
        type Requests = Arc<Mutex<Vec<Value>>>;
        async fn respond(
            State(requests): State<Requests>,
            headers: HeaderMap,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            assert_eq!(headers["x-api-key"], "fake-messages");
            assert_eq!(headers["anthropic-version"], "2023-06-01");
            let mut requests = requests.lock().unwrap();
            requests.push(body);
            let index = requests.len();
            let (content, stop) = match index {
                1 => (
                    json!([{"type":"text","text":"# Episode\nBlue facts."}]),
                    "end_turn",
                ),
                2 => (
                    json!([{"type":"text","text":"Classical blue facts."}]),
                    "end_turn",
                ),
                3 => (
                    json!([{"type":"text","text":"# Chapter\nBlue facts.\nRelated: none"}]),
                    "end_turn",
                ),
                4 => (
                    json!([{"type":"thinking","thinking":"Look up the original","signature":"opaque-signature"},{"type":"tool_use","id":"haiku-search","name":"history_search","input":{"query":"blue","limit":1}}]),
                    "tool_use",
                ),
                5 => (json!([{"type":"text","text":"Blue."}]), "end_turn"),
                6 => (
                    json!([{"type":"text","text":"{\"grade\":\"supported\",\"reason\":\"Blue is supported.\"}"}]),
                    "end_turn",
                ),
                7 => (
                    json!([{"type":"text","text":"{\"choice\":\"none\"}"}]),
                    "end_turn",
                ),
                _ => {
                    return Json(
                        json!({"content":[{"type":"text","text":"Unknown usage"}],"stop_reason":"end_turn"}),
                    );
                }
            };
            // Whole prompt is 100001 although uncached input alone is 20001.
            Json(
                json!({"model":"claude-haiku-5-5","content":content,"stop_reason":stop,"usage":{"input_tokens":20_001,"cache_read_input_tokens":60_000,"cache_creation_input_tokens":20_000,"output_tokens":2_000}}),
            )
        }
        let requests: Requests = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let app = Router::new()
            .route("/messages", post(respond))
            .with_state(requests.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let directory = tempfile::tempdir()?;
        let meter = Arc::new(Mutex::new(Telemetry {
            max_cost_usd: 5.0,
            ..Default::default()
        }));
        let mut config = config_for_decider(&format!("llm/{HAIKU_MODEL}"));
        config.roles.summary.models = vec![HAIKU_MODEL.into()];
        config.roles.summary.reasoning_effort = ReasoningEffort::High;
        config.roles.chapter.models = vec![HAIKU_MODEL.into()];
        config.roles.chapter.reasoning_effort = ReasoningEffort::Medium;
        config.roles.decider.reasoning_effort = ReasoningEffort::Medium;
        config.roles.decider.repeats = Some(1);
        let mut endpoints = ProviderEndpoints {
            opencode_base_url: None,
            opencode_messages_base_url: Some(base),
            ..Default::default()
        };
        let mut keys = ProviderKeys {
            opencode: Some("fake-messages".into()),
            ..Default::default()
        };
        let proxy = relay_configured_keys(
            &mut config,
            &mut endpoints,
            meter.clone(),
            directory.path().join("calls.jsonl"),
            example_prices(),
            &mut keys,
        )
        .await?;
        assert_eq!(config.roles.summary.models, vec![HAIKU_MODEL]);
        let mut registry = ProviderRegistry::new(
            "replay",
            Arc::new(ScriptedProvider::new("must not be called")),
        );
        endpoints.register_with_keys(&mut registry, keys)?;
        let registry = Arc::new(registry);
        let models = HttpEpisodeModels::from_registry(config.clone(), registry.clone(), None)?;
        meter.lock().unwrap().begin_operation("summary");
        let summary = models
            .summarize(SummaryInput {
                focus: None,
                session_id: "local".into(),
                previous_summary: None,
                turn_request: None,
                episode_text: "Blue facts.".into(),
                cap: 100,
            })
            .await?;
        assert_eq!(summary.usage.input_tokens, 100_001);
        assert_eq!(summary.usage.cache_read_tokens, 60_000);
        assert_eq!(summary.usage.cache_creation_tokens, 20_000);
        let aggregate_usage = summary.usage.clone();
        meter.lock().unwrap().begin_operation("summary");
        models
            .summarize_classical(ClassicalInput {
                session_id: "local".into(),
                previous_summary: None,
                latest_user_request: None,
                span_text: "Blue facts.".into(),
            })
            .await?;
        meter.lock().unwrap().begin_operation("chapter");
        models
            .close_chapter(ChapterInput {
                session_id: "local".into(),
                episodes: vec![summary.summary],
                previous_chapters: vec![],
                typed_user_texts: vec![],
                task_instructions: Vec::new(),
            })
            .await?;
        let probe_model = Arc::new(ReplayProbeModel {
            prices: example_prices(),
            context_route: Default::default(),
            telemetry: meter.clone(),
            url: None,
            client: reqwest::Client::new(),
            model: HAIKU_MODEL.into(),
            effort: ReasoningEffort::High,
            registry: Some(registry),
        });
        let store = Store::open(directory.path().join("probe.sqlite"))?;
        let session = store.create_session("probe")?;
        let source = Message {
            id: "persisted-source".into(),
            session_id: session.id.clone(),
            role: Role::User,
            origin: None,
            content: "Blue facts.".into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: String::new(),
        };
        store.append_batch(std::slice::from_ref(&source))?;
        store.save_context_checkpoint(&bubblepaw_core::ContextCheckpoint {
            id: "cache-usage-proof".into(),
            session_id: session.id.clone(),
            through_message_id: source.id,
            model: HAIKU_MODEL.into(),
            summary: "Aggregated episode summary.".into(),
            files_touched: vec![],
            created_at: String::new(),
            usage: Some(aggregate_usage),
        })?;
        let reopened = Store::open(directory.path().join("probe.sqlite"))?;
        let persisted = reopened
            .context_checkpoint(&session.id)?
            .unwrap()
            .usage
            .unwrap();
        assert_eq!(persisted.input_tokens, 100_001);
        assert_eq!(persisted.output_tokens, 2_000);
        assert_eq!(persisted.cache_read_tokens, 60_000);
        assert_eq!(persisted.cache_creation_tokens, 20_000);
        let probe: crate::probes::Probe = serde_json::from_value(
            json!({"id":"p","type":"recall","question":"What colour?","expected_answer":"Blue","evidence_message_ids":[],"earliest_checkpoint":1}),
        )?;
        let answer = crate::probes::answer(
            probe_model.clone(),
            &store,
            &session.id,
            &ModelRequest::default(),
            &probe,
        )
        .await;
        assert_eq!(answer["status"], "answered");
        assert_eq!(answer["tool_calls"].as_array().unwrap().len(), 1);
        let judgment = crate::probes::judge(probe_model.as_ref(), &probe, "Blue.").await;
        assert_eq!(judgment["status"], "graded");
        meter.lock().unwrap().begin_operation("decision");
        models
            .decide(SplitInput {
                session_id: "local".into(),
                open_chapter: "Blue".into(),
                options: vec![crate::episode_models::SplitOption {
                    message_id: "u1".into(),
                    user: "continue".into(),
                    agent_before: String::new(),
                }],
            })
            .await?;
        {
            let recorded = requests.lock().unwrap();
            assert_eq!(recorded.len(), 7);
            for (index, request) in recorded.iter().enumerate() {
                assert_eq!(request["model"], "claude-haiku-5-5");
                assert_eq!(request["thinking"]["type"], "adaptive");
                assert_eq!(
                    request["output_config"]["effort"],
                    if matches!(index, 2 | 6) {
                        "medium"
                    } else {
                        "high"
                    }
                );
            }
            assert_eq!(
                recorded[3]["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|tool| tool["name"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                vec!["call_tool", "history_read", "history_search", "tool_search"]
            );
            assert_eq!(
                recorded[4]["messages"][1]["content"][0]["signature"],
                "opaque-signature"
            );
            assert_eq!(
                recorded[4]["messages"][2]["content"][0]["type"],
                "tool_result"
            );
            assert!(recorded[5]["tools"].is_null());
        }
        // The maximum output reservation also covers this synthetic long-prompt usage.
        {
            let meter = meter.lock().unwrap();
            assert!((meter.haiku_cost_usd - 7.0 * 0.0305005).abs() < 1e-12);
            assert_eq!(meter.flash_cost_usd, 0.0);
            assert_eq!(meter.calls[0]["input_tokens"], 100_001);
            assert_eq!(meter.calls[0]["cache_creation_tokens"], 20_000);
            assert!(meter.stopped.is_none());
        }
        probe_model
            .call(
                json!({"messages":[{"role":"user","content":"uncertain"}],"max_tokens":65536}),
                "probe",
            )
            .await?;
        assert!(meter.lock().unwrap().reserved_unknown_cost_usd > 0.0);
        proxy.abort();
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn unavailable_haiku_summary_falls_back_to_flash_for_summary_probe_and_judge()
    -> Result<()> {
        use axum::{Json, Router, extract::State, routing::post};
        type Requests = Arc<Mutex<Vec<Value>>>;
        async fn respond(State(requests): State<Requests>, Json(body): Json<Value>) -> Json<Value> {
            let mut requests = requests.lock().unwrap();
            requests.push(body);
            let text = if requests.len() == 3 {
                "{\"grade\":\"supported\",\"reason\":\"Blue is supported.\"}"
            } else {
                "Blue."
            };
            Json(
                json!({"choices":[{"message":{"role":"assistant","content":text},"finish_reason":"stop"}],"usage":{"prompt_tokens":17,"completion_tokens":2}}),
            )
        }
        let requests: Requests = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let app = Router::new()
            .route("/chat/completions", post(respond))
            .with_state(requests.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let directory = tempfile::tempdir()?;
        let meter = Arc::new(Mutex::new(Telemetry::default()));
        let mut config = config_for_decider(&format!("llm/{FLASH_MODEL}"));
        config.roles.summary.models = vec![HAIKU_MODEL.into(), FLASH_MODEL.into()];
        config.roles.chapter.models = config.roles.summary.models.clone();
        config.roles.summary.reasoning_effort = ReasoningEffort::High;
        let mut endpoints = ProviderEndpoints {
            opencode_base_url: Some(base),
            opencode_messages_base_url: None,
            ..Default::default()
        };
        let mut keys = ProviderKeys {
            opencode: Some("fake".into()),
            ..Default::default()
        };
        let proxy = relay_configured_keys(
            &mut config,
            &mut endpoints,
            meter.clone(),
            directory.path().join("calls.jsonl"),
            example_prices(),
            &mut keys,
        )
        .await?;
        let mut registry = ProviderRegistry::new(
            "replay",
            Arc::new(ScriptedProvider::new("must not be called")),
        );
        endpoints.register_with_keys(&mut registry, keys)?;
        let registry = Arc::new(registry);
        assert_eq!(config.roles.summary.models, vec![HAIKU_MODEL, FLASH_MODEL]);
        let selected = config
            .roles
            .summary
            .models
            .iter()
            .find(|model| registry.llm_available(model))
            .unwrap()
            .clone();
        assert_eq!(selected, FLASH_MODEL);
        let models = HttpEpisodeModels::from_registry(config.clone(), registry.clone(), None)?;
        meter.lock().unwrap().begin_operation("summary");
        models
            .summarize_classical(ClassicalInput {
                session_id: "local".into(),
                previous_summary: None,
                latest_user_request: None,
                span_text: "Blue facts".into(),
            })
            .await?;
        let model = Arc::new(ReplayProbeModel {
            prices: example_prices(),
            context_route: Default::default(),
            telemetry: meter.clone(),
            url: Some(format!(
                "{}/chat/completions",
                endpoints.opencode_base_url.unwrap()
            )),
            client: reqwest::Client::new(),
            model: selected,
            effort: config.roles.summary.reasoning_effort,
            registry: Some(registry),
        });
        let store = Store::open(directory.path().join("probe.sqlite"))?;
        let session = store.create_session("probe")?;
        let probe: crate::probes::Probe = serde_json::from_value(
            json!({"id":"p","type":"recall","question":"What colour?","expected_answer":"Blue","evidence_message_ids":[],"earliest_checkpoint":1}),
        )?;
        let frozen = ModelRequest {
            messages: vec![Message {
                id: "u1".into(),
                session_id: session.id.clone(),
                role: Role::User,
                origin: None,
                content: "Blue facts".into(),
                tool_calls: vec![],
                tool_call_id: None,
                provider_state: Some(
                    json!({"adapter":"responses","items":[{"encrypted_content":"opaque encrypted state"}]}),
                ),
                is_error: false,
                created_at: String::new(),
            }],
            ..Default::default()
        };
        let answer =
            crate::probes::answer(model.clone(), &store, &session.id, &frozen, &probe).await;
        assert_eq!(answer["status"], "answered");
        assert_eq!(
            crate::probes::judge(model.as_ref(), &probe, "Blue.").await["status"],
            "graded"
        );
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 3);
            for request in requests.iter() {
                assert_eq!(request["model"], "deepseek-v4.1-flash");
                assert_eq!(request["reasoning_effort"], "high");
                assert!(!request.to_string().contains("opaque encrypted state"));
            }
        }
        assert_eq!(meter.lock().unwrap().haiku_cost_usd, 0.0);
        proxy.abort();
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn selected_luna_and_flash_deciders_are_relayed_with_only_their_credentials() -> Result<()>
    {
        use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
        use std::collections::VecDeque;
        type Replies = Arc<Mutex<VecDeque<(StatusCode, Value)>>>;
        async fn respond(
            State(replies): State<Replies>,
            Json(body): Json<Value>,
        ) -> (StatusCode, Json<Value>) {
            assert!(body["model"] == "example-decider" || body["model"] == "deepseek-v4.1-flash");
            let (status, payload) = replies.lock().unwrap().pop_front().unwrap();
            (status, Json(payload))
        }
        for luna in [true, false] {
            let payload = if luna {
                json!({"model":"example-decider","answers":[{"name":"choice","type":"choice","choice":"none","probabilities":[{"value":"1","probability":0.0},{"value":"none","probability":1.0}]}],"usage":{"input_tokens":11,"output_tokens":999}})
            } else {
                json!({"choices":[{"message":{"content":"{\"choice\":\"none\"}"}}],"usage":{"prompt_tokens":11,"completion_tokens":2}})
            };
            // Luna is calibrated to one sample per split; Flash keeps three.
            let samples = if luna { 1 } else { 3 };
            let mut replies = VecDeque::from([(StatusCode::TOO_MANY_REQUESTS, json!({}))]);
            replies.extend(std::iter::repeat_n((StatusCode::OK, payload), samples));
            let replies: Replies = Arc::new(Mutex::new(replies));
            let queue = replies.clone();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let base = format!("http://{}", listener.local_addr()?);
            let server = tokio::spawn(async move {
                axum::serve(
                    listener,
                    Router::new()
                        .route("/v1/decisions", post(respond))
                        .route("/chat/completions", post(respond))
                        .with_state(replies),
                )
                .await
                .unwrap();
            });
            let directory = tempfile::tempdir()?;
            let meter = Arc::new(Mutex::new(Telemetry::default()));
            // Keep Luna first even without its key: metering must follow the
            // model that is actually available, just like split calibration.
            let mut config = EpisodeModelConfig {
                retry_base_delay: Duration::ZERO,
                ..Default::default()
            };
            config.roles.decider.models = vec![
                "openai/example-decider".into(),
                "llm/opencode/deepseek-v4.1-flash".into(),
            ];
            // Calibration for the metered OpenAI decider is explicit here: the
            // production default applies only to the shipped model name.
            config.roles.decider.repeats = luna.then_some(1);
            config.roles.decider.threshold = luna.then_some(0.6);
            let mut endpoints = ProviderEndpoints {
                opencode_base_url: Some(base.clone()),
                openai_base_url: base,
                ..Default::default()
            };
            let mut keys = ProviderKeys {
                opencode: Some("fake-summary".into()),
                openrouter: Some("unused-router-key".into()),
                openai: luna.then(|| "fake-openai".into()),
            };
            let proxy = relay_configured_keys(
                &mut config,
                &mut endpoints,
                meter.clone(),
                directory.path().join("calls.jsonl"),
                example_prices(),
                &mut keys,
            )
            .await?;
            assert!(keys.openrouter.is_none());
            assert_eq!(keys.openai.is_some(), luna);
            assert_eq!(
                config.roles.decider.models,
                vec![if luna {
                    "openai/example-decider"
                } else {
                    "llm/opencode/deepseek-v4.1-flash"
                }]
            );
            let mut registry = ProviderRegistry::new(
                "replay",
                Arc::new(ScriptedProvider::new("must not be called")),
            );
            endpoints.register_with_keys(&mut registry, keys)?;
            let models = HttpEpisodeModels::from_registry(config, Arc::new(registry), None)?;
            meter.lock().unwrap().begin_operation("decision");
            let decision = models
                .decide(SplitInput {
                    session_id: "local".into(),
                    open_chapter: "facts".into(),
                    options: vec![crate::episode_models::SplitOption {
                        message_id: "u1".into(),
                        user: "continue".into(),
                        agent_before: String::new(),
                    }],
                })
                .await?;
            assert_eq!(decision.choice, None);
            let input = 11 * samples as u64;
            assert_eq!(decision.usage.input_tokens, input);
            // Physical usage preserves reported tokens even though Luna's relay
            // billing below prices only input and records zero billable output.
            assert_eq!(decision.usage.output_tokens, if luna { 999 } else { 6 });
            {
                let meter = meter.lock().unwrap();
                assert_eq!(meter.calls.len(), samples + 1);
                assert_eq!(meter.retries, 1);
                assert_eq!(meter.calls[1]["transport_retry"], true);
                assert_eq!(meter.calls[samples]["request_slot"], samples);
                assert_eq!(meter.jev_cost_usd, 0.0);
                if luna {
                    assert!((meter.openai_cost_usd - input as f64 * 0.00000010).abs() < 1e-12);
                    assert_eq!(meter.flash_cost_usd, 0.0);
                    assert_eq!(meter.calls[1]["output_tokens"], 999);
                } else {
                    assert!(
                        (meter.flash_cost_usd - (33.0 * 0.0000003 + 6.0 * 0.0000012)).abs() < 1e-12
                    );
                    assert_eq!(meter.openai_cost_usd, 0.0);
                }
            }
            assert!(queue.lock().unwrap().is_empty());
            proxy.abort();
            server.abort();
        }
        Ok(())
    }
}
