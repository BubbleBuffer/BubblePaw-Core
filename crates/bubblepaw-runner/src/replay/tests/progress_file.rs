//! The metadata-only progress file.
use super::*;

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
