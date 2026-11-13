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
#[path = "replay_archive.rs"]
mod archive;
#[cfg(test)]
use archive::ARCHIVE_UNKNOWN_OUTCOME;
pub(crate) use archive::normalize_corpus;
use archive::{ARCHIVE_NORMALIZATION_POLICY, ArchiveNormalization, normalize_archive};
#[path = "replay_telemetry.rs"]
mod telemetry;
use telemetry::Telemetry;
#[path = "replay_model_set.rs"]
mod model_set;
use model_set::{ReplayModels, scripted_episode_memory};
#[path = "replay_plan.rs"]
mod plan;
pub use plan::build_probe_plan;
use plan::{
    checkpoint_mismatch, default_replay_summary_config, model_signature, replay_summary_config,
    resolved_threshold, turn_end_steps, turn_snapshot, validate_plan,
};
#[path = "replay_progress.rs"]
mod progress;
use progress::{PROGRESS_INTERVAL_MESSAGES, ProgressWriter};
#[path = "replay_report.rs"]
mod report;
pub use report::boundary_score;
use report::{
    accepted_boundaries, call_totals, markdown, physical_storage, projected_group, related_score,
    role_input_tokens, string_array, token_count,
};

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

#[path = "replay_probe_model.rs"]
mod probe_transport;
use probe_transport::ReplayProbeModel;

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
    let source_corpus_raw_sha256 = bubblepaw_core::fingerprint(&std::fs::read(corpus_path)?);
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

/// Test hook: every prepared step request, keyed by committed history length.
#[cfg(test)]
type PreparedTrace<'a> = Option<&'a Mutex<Vec<(usize, ModelRequest)>>>;

#[path = "replay_state.rs"]
mod run_state;
use run_state::ReplayRun;
#[path = "replay_checkpoint.rs"]
mod checkpoint;
#[path = "replay_finish.rs"]
mod finish;
#[path = "replay_step.rs"]
mod step;
#[path = "replay_views.rs"]
mod views;

async fn run_corpus_inner(
    corpus: ReplayCorpus,
    options: ReplayOptions,
    config: EpisodeModelConfig,
    endpoints: ProviderEndpoints,
    summary_config: SummaryConfig,
    execution: ReplayExecution,
    #[cfg(test)] prepared_trace: PreparedTrace<'_>,
) -> Result<Value> {
    let (mut run, corpus) = ReplayRun::open(
        corpus,
        options,
        config,
        endpoints,
        summary_config,
        execution,
    )
    .await?;
    for message in &corpus.messages {
        let step = run
            .step(
                &corpus,
                message,
                #[cfg(test)]
                prepared_trace,
            )
            .await?;
        if step.is_break() {
            break;
        }
    }
    run.finish(
        &corpus,
        #[cfg(test)]
        prepared_trace,
    )
    .await
}

#[cfg(test)]
mod telemetry_tests;

#[cfg(test)]
mod tests;
