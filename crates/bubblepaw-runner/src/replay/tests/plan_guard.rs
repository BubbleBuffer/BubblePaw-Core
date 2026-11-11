//! Checkpoint guard and common plan consumers.
use super::*;

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
