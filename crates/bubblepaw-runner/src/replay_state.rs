//! Replay run state: input validation and setup before the message loop.
use super::progress::ReplayHeartbeat;
use super::*;
use crate::probes::{Probe, ProbePlan};

/// State shared by the per-message loop and the final report.
pub(super) struct ReplayRun {
    pub(super) options: ReplayOptions,
    pub(super) execution: ReplayExecution,
    pub(super) started: Instant,
    pub(super) archive_normalization: ArchiveNormalization,
    pub(super) corpus_sha256: String,
    pub(super) pinned_plan: Option<(ProbePlan, String)>,
    pub(super) probes: Vec<Probe>,
    pub(super) turn_steps: Vec<usize>,
    pub(super) threshold: usize,
    pub(super) episode_policy: EpisodeConfig,
    pub(super) store: Store,
    pub(super) session_id: String,
    pub(super) estimator: Arc<SummarizingContext>,
    pub(super) telemetry: Arc<Mutex<Telemetry>>,
    pub(super) relay_task: Option<tokio::task::JoinHandle<()>>,
    pub(super) probe_model: Arc<ReplayProbeModel>,
    pub(super) worker: EpisodeWorker,
    pub(super) context_state: bubblepaw_core::ContextState,
    pub(super) importer: Store,
    pub(super) history: Vec<Message>,
    pub(super) activated: bool,
    pub(super) pending_calls: HashSet<String>,
    pub(super) imports: ReplayImportBatch,
    pub(super) worker_gate: ReplayWorkerGate,
    pub(super) compactions: Vec<Value>,
    pub(super) probe_rows: Vec<Value>,
    pub(super) turn_frontiers: Vec<Value>,
    pub(super) plan_mismatches: Vec<Value>,
    pub(super) view_tokens_per_turn: Vec<Value>,
    pub(super) failures: Vec<Value>,
    pub(super) stopped: Option<String>,
    pub(super) active_tokens: usize,
    pub(super) progress: ProgressWriter,
    pub(super) archive_setup_seconds: f64,
    _heartbeat: ReplayHeartbeat,
}

/// Inputs checked and resolved before any artifact is created.
struct Validated {
    archive_normalization: ArchiveNormalization,
    corpus_sha256: String,
    pinned_plan: Option<(ProbePlan, String)>,
    probes: Vec<Probe>,
    turn_steps: Vec<usize>,
    started: Instant,
}

type LiveModels = (
    Option<Arc<dyn EpisodeModels>>,
    Option<tokio::task::JoinHandle<()>>,
    Option<Arc<ProviderRegistry>>,
);

fn validate(
    corpus: &mut ReplayCorpus,
    options: &mut ReplayOptions,
    config: &EpisodeModelConfig,
    endpoints: &ProviderEndpoints,
    summary_config: &SummaryConfig,
) -> Result<Validated> {
    ensure!(
        !options.plan_probes,
        "Use build_probe_plan or the replay planning command"
    );
    let archive_normalization = normalize_archive(corpus)?;
    let corpus_sha256 = archive_normalization.source_corpus_sha256.clone();
    let threshold = resolved_threshold(corpus, options, summary_config);
    let model_signature = model_signature(config, endpoints, summary_config, threshold);
    let pinned_plan = if let Some(path) = &options.probe_plan {
        ensure!(
            options.probes.is_none()
                && options.checkpoints_at_turns.is_empty()
                && options.probe_limit.is_none(),
            "--probe-plan supplies probes, positions and limit; reject per-arm overrides"
        );
        let (plan, digest) = crate::probes::load_plan(path)?;
        validate_plan(&plan, corpus, threshold, &model_signature)?;
        options.checkpoints_at_turns = plan.checkpoints_at_turns.clone();
        // Pinned comparisons archive checkpoint hashes and compact metrics, not
        // hundreds of redundant human-readable serving-view dumps.
        options.capture_turn_frontiers = true;
        Some((plan, digest))
    } else {
        None
    };
    replay_roles(
        config,
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
    Ok(Validated {
        archive_normalization,
        corpus_sha256,
        pinned_plan,
        probes,
        turn_steps,
        started,
    })
}

/// The recorded-call relay and episode models; scripted runs have none.
async fn live_models(
    config: &mut EpisodeModelConfig,
    endpoints: &mut ProviderEndpoints,
    options: &ReplayOptions,
    telemetry: &Arc<Mutex<Telemetry>>,
    store: &Store,
    fallback: &Arc<ScriptedProvider>,
    episode_policy: &EpisodeConfig,
) -> Result<LiveModels> {
    if options.dry_run {
        return Ok((None, None, None));
    }
    let (task, keys) = relay(
        config,
        endpoints,
        telemetry.clone(),
        options.out.join("calls.jsonl"),
        options.prices,
        episode_policy.decider_enabled,
    )
    .await?;
    let mut registry = ProviderRegistry::new("replay", fallback.clone());
    registry.set_usage_store(store.clone());
    endpoints.register_with_keys(&mut registry, keys)?;
    let registry = Arc::new(registry);
    let inner: Arc<dyn EpisodeModels> = Arc::new(
        HttpEpisodeModels::from_registry(config.clone(), registry.clone(), None)?
            .with_episode_config(episode_policy.clone())?,
    );
    Ok((Some(inner), Some(task), Some(registry)))
}

fn probe_model(
    config: &EpisodeModelConfig,
    endpoints: &ProviderEndpoints,
    options: &ReplayOptions,
    telemetry: &Arc<Mutex<Telemetry>>,
    probe_registry: Option<Arc<ProviderRegistry>>,
) -> Result<Arc<ReplayProbeModel>> {
    Ok(Arc::new(ReplayProbeModel {
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
    }))
}

impl ReplayRun {
    pub(super) async fn open(
        mut corpus: ReplayCorpus,
        mut options: ReplayOptions,
        mut config: EpisodeModelConfig,
        mut endpoints: ProviderEndpoints,
        summary_config: SummaryConfig,
        execution: ReplayExecution,
    ) -> Result<(Self, ReplayCorpus)> {
        let Validated {
            archive_normalization,
            corpus_sha256,
            pinned_plan,
            probes,
            turn_steps,
            started,
        } = validate(
            &mut corpus,
            &mut options,
            &config,
            &endpoints,
            &summary_config,
        )?;
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
        // Scripted runs also reject unpriced configured roles before making artifacts.
        replay_roles(&config, options.prices, episode_policy.decider_enabled)?;
        let (inner, relay_task, probe_registry) = live_models(
            &mut config,
            &mut endpoints,
            &options,
            &telemetry,
            &store,
            &fallback,
            &episode_policy,
        )
        .await?;
        let probe_model = probe_model(&config, &endpoints, &options, &telemetry, probe_registry)?;
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
        let importer = store.clone();
        let archive_setup_seconds = archive_setup_started.elapsed().as_secs_f64();
        let history = Vec::new();
        let activated = false;
        let pending_calls = HashSet::new();
        let imports = ReplayImportBatch::default();
        let worker_gate = ReplayWorkerGate::with_config(episode_policy.clone());
        let compactions = Vec::new();
        let probe_rows = Vec::new();
        let turn_frontiers = Vec::new();
        let plan_mismatches = Vec::new();
        let view_tokens_per_turn = Vec::new();
        let failures = Vec::new();
        let stopped = None;
        let active_tokens = estimator.estimated_tokens(&ModelRequest {
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
        Ok((
            Self {
                options,
                execution,
                started,
                archive_normalization,
                corpus_sha256,
                pinned_plan,
                probes,
                turn_steps,
                threshold,
                episode_policy,
                store,
                session_id: session.id,
                estimator,
                telemetry,
                relay_task,
                probe_model,
                worker,
                context_state,
                importer,
                history,
                activated,
                pending_calls,
                imports,
                worker_gate,
                compactions,
                probe_rows,
                turn_frontiers,
                plan_mismatches,
                view_tokens_per_turn,
                failures,
                stopped,
                active_tokens,
                progress,
                archive_setup_seconds,
                _heartbeat,
            },
            corpus,
        ))
    }
}
