//! Replay summary configuration, plan validation and the probe plan builder.
use super::*;

pub(super) fn default_replay_summary_config() -> SummaryConfig {
    // Archive replay has no live main request to fork. This constructor selects
    // source-only input explicitly; callers supplying context config own that policy.
    let mut config = SummaryConfig::default();
    config.lcm.episodes.summary_input = crate::episodes::EpisodeSummaryInput::Episode;
    config
}

pub(super) fn replay_summary_config(
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

pub(super) fn resolved_threshold(
    corpus: &ReplayCorpus,
    options: &ReplayOptions,
    summary: &SummaryConfig,
) -> usize {
    replay_summary_config(corpus, options, summary).threshold()
}
pub(super) fn turn_end_steps(history: &[Message]) -> Vec<usize> {
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
pub(super) fn model_signature(
    config: &EpisodeModelConfig,
    endpoints: &ProviderEndpoints,
    summary: &SummaryConfig,
    threshold: usize,
) -> Value {
    let role = |role: &crate::model_roles::ModelRole| json!({"models":role.models,"repeats":role.repeats,"threshold":role.threshold,"reasoning_effort":role.reasoning_effort});
    json!({"probe":crate::probes::metadata_for(&config.roles.summary.models[0], config.roles.summary.reasoning_effort),"summary":role(&config.roles.summary),"chapter":role(&config.roles.chapter),"decider":role(&config.roles.decider),"decider_enabled":summary.lcm.episodes.decider_enabled,"threshold":threshold,"verbatim_user_tokens":summary.verbatim_user_tokens,"provider_reasoning":format!("{:?}",endpoints)})
}
pub(super) fn validate_plan(
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
pub(super) fn turn_snapshot(
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

pub(super) fn checkpoint_mismatch(
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
