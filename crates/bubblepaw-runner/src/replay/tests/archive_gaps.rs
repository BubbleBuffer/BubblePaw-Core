//! Archive normalization of recorded tool calls without outcomes.
use super::*;

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
        normalize_archive(&mut gap_corpus(vec![calls, duplicate_result, result_again])).is_err()
    );
    Ok(())
}

#[tokio::test]
async fn archive_unknown_outcomes_pin_identical_planner_and_answering_positions() -> Result<()> {
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
