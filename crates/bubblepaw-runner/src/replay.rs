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
                    } else if item.content.contains("history_search") {
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

#[cfg(test)]
mod telemetry_tests;

#[cfg(test)]
mod tests;
