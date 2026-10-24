#![cfg(feature = "bench")]

use bubblepaw_core::{ContextStrategy, Message, MessageOrigin, Role, Store};
use bubblepaw_runner::pricing::PriceTable;
use bubblepaw_runner::replay::{ReplayCorpus, ReplayOptions, boundary_score, run_corpus};
use serde_json::json;
use std::collections::HashMap;

fn message(index: usize, role: Role, content: String) -> Message {
    Message {
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
    }
}

#[tokio::test]
async fn small_scripted_replay_drives_worker_compaction_and_keeps_canonical_history()
-> anyhow::Result<()> {
    let output = tempfile::tempdir()?;
    let mut messages = Vec::new();
    for turn in 0..4 {
        messages.push(message(
            turn * 2 + 1,
            Role::User,
            if turn == 1 {
                "[new subject] Please start another task.".into()
            } else {
                "Please continue this task.".into()
            },
        ));
        // Three complete turns suffice to close two 32k episodes and cross the
        // 80k fixture threshold. The last turn remains production's raw remainder.
        messages.push(message(
            turn * 2 + 2,
            Role::Assistant,
            "transient ".repeat(36_000),
        ));
    }
    let corpus = ReplayCorpus {
        chat_id: "small-fixture".into(),
        recorded_model: None,
        context_window: None,
        instructions: "Fixture instructions".into(),
        messages,
        message_metadata: json!([]),
        boundary_gold: Some(json!({"source":"fixture","gold":["r3"],"ambiguous":[],"chapters":[]})),
        relates_gold: vec![],
        provenance: json!({"fixture":true}),
    };
    let report = run_corpus(
        corpus,
        ReplayOptions {
            prices: PriceTable::shared(),
            agents: Default::default(),
            chat: "fixture".into(),
            out: output.path().into(),
            threshold: Some(80_000),
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
    assert_eq!(report["status"], "complete");
    assert!(report["episodes"].as_array().unwrap().len() >= 2);
    assert!(report["chapters"].as_array().unwrap().len() >= 2);
    assert!(!report["compactions"].as_array().unwrap().is_empty());
    assert_eq!(report["boundary_scores"]["exact"]["f1"], 1.0);
    assert_eq!(report["totals"]["estimated_cost_usd"], 0.0);
    assert_eq!(report["storage"]["message_count"], 8);
    assert_eq!(
        report["storage"]["transaction_count"].as_u64().unwrap(),
        report["storage_transaction_basis"]["store_transactions"]
            .as_u64()
            .unwrap()
            + report["storage_transaction_basis"]["canonical_import_transactions"]
                .as_u64()
                .unwrap()
    );
    assert!(
        report["storage_transaction_basis"]["canonical_import_transactions"]
            .as_u64()
            .unwrap()
            < 8
    );
    assert!(output.path().join("report.md").is_file());
    assert!(output.path().join("report.json").is_file());
    let view = &report["compactions"][0];
    assert!(
        output
            .path()
            .join(view["view_file"].as_str().unwrap())
            .is_file()
    );
    let items = view["items"].as_array().unwrap();
    let item_total: u64 = items.iter().map(|i| i["tokens"].as_u64().unwrap()).sum();
    // The served request carries the view's cold notes; the planned view tokens do not count them.
    let notes: u64 = items
        .iter()
        .filter(|i| {
            i["message_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("context:cold_notes:"))
        })
        .map(|i| i["tokens"].as_u64().unwrap())
        .sum();
    assert!(notes > 0, "the compaction request carries the cold notes");
    assert_eq!(item_total, view["tokens"].as_u64().unwrap() + notes);
    let store = Store::open_data_dir(output.path().join("store"))?;
    assert_eq!(
        store
            .messages(report["session_id"].as_str().unwrap())?
            .len(),
        8
    );
    Ok(())
}

#[test]
fn tolerant_scoring_reuses_pipeline_maximum_matching_and_ambiguous_credit() {
    let ordinals = HashMap::from([("a", 0), ("b", 1), ("c", 2), ("d", 20)]);
    let gold = vec!["a".into(), "c".into()];
    let predicted = vec!["b".into(), "d".into()];
    let exact = boundary_score(&predicted, &gold, &ordinals, &[], false);
    assert_eq!(exact["tp"], 0);
    let tolerant = boundary_score(&predicted, &gold, &ordinals, &["d".into()], true);
    assert_eq!(tolerant["tp"], 2);
    assert_eq!(tolerant["f1"], 1.0);
    assert_eq!(tolerant["matching"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn unplanned_probes_are_rejected_before_any_store_or_provider_dispatch() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let corpus = ReplayCorpus {
        chat_id: "unplanned-fixture".into(),
        recorded_model: None,
        context_window: None,
        instructions: String::new(),
        messages: vec![message(1, Role::User, "BubblePlay was blue".into())],
        message_metadata: json!([]),
        boundary_gold: None,
        relates_gold: vec![],
        provenance: json!({"fixture":true}),
    };
    for dry_run in [true, false] {
        let out = directory
            .path()
            .join(if dry_run { "scripted" } else { "paid" });
        let result = run_corpus(
            corpus.clone(),
            ReplayOptions {
                prices: PriceTable::shared(),
                agents: Default::default(),
                chat: "fixture".into(),
                out: out.clone(),
                threshold: Some(80_000),
                dry_run,
                context_strategy: ContextStrategy::Lcm,
                probes: Some(directory.path().join("not-read.json")),
                checkpoints_at_turns: vec![1],
                probe_plan: None,
                plan_probes: false,
                capture_turn_frontiers: false,
                probe_limit: Some(1),
                max_cost_usd: None,
            },
        )
        .await;
        let error = result.expect_err("Unplanned answering must reject before any paid dispatch");
        assert!(error.to_string().contains("all answering runs"));
        // Validation precedes archive creation, model construction and the spend relay.
        assert!(
            !out.exists(),
            "No session, journal or paid call may be created"
        );
    }
    Ok(())
}

#[tokio::test]
async fn scripted_comparison_arms_report_context_metrics() -> anyhow::Result<()> {
    for strategy in [ContextStrategy::Classical, ContextStrategy::Lcm] {
        let output = tempfile::tempdir()?;
        let mut messages = Vec::new();
        for turn in 0..4 {
            messages.push(message(
                turn * 2 + 1,
                Role::User,
                format!("Please continue task {turn}."),
            ));
            messages.push(message(
                turn * 2 + 2,
                Role::Assistant,
                "transient ".repeat(36_000),
            ));
        }
        let corpus = ReplayCorpus {
            chat_id: "comparison-fixture".into(),
            recorded_model: None,
            context_window: None,
            instructions: "Fixture instructions".into(),
            messages,
            message_metadata: json!([]),
            boundary_gold: None,
            relates_gold: vec![],
            provenance: json!({"fixture":true}),
        };
        let report = run_corpus(
            corpus,
            ReplayOptions {
                prices: PriceTable::shared(),
                agents: Default::default(),
                chat: "fixture".into(),
                out: output.path().into(),
                threshold: Some(80_000),
                dry_run: true,
                context_strategy: strategy,
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
        assert_eq!(report["status"], "complete");
        assert_eq!(report["context_strategy"], serde_json::to_value(strategy)?);
        assert!(report["compaction_count"].as_u64().unwrap() > 0);
        assert_eq!(report["view_tokens_per_turn"].as_array().unwrap().len(), 4);
        assert_eq!(report["final_view"]["extra_model_calls"], 0);
        assert!(output.path().join("final-view.txt").is_file());
        assert_eq!(report["totals"]["estimated_cost_usd"], 0.0);
        assert!(report["totals"]["summary"]["calls"].as_u64().unwrap() > 0);
        assert!(
            report["totals"]["summary"]["input_tokens"]
                .as_u64()
                .unwrap()
                > 0
        );
        if strategy == ContextStrategy::Classical {
            assert!(report["episodes"].as_array().unwrap().is_empty());
            assert!(report["chapters"].as_array().unwrap().is_empty());
            assert!(report["rolling_checkpoint"].is_object());
        } else {
            assert!(!report["episodes"].as_array().unwrap().is_empty());
            assert!(report["rolling_checkpoint"].is_null());
        }
        let store = Store::open_data_dir(output.path().join("store"))?;
        assert_eq!(
            store
                .session(report["session_id"].as_str().unwrap())?
                .context_strategy,
            strategy
        );
        assert_eq!(
            store
                .messages(report["session_id"].as_str().unwrap())?
                .len(),
            8
        );
    }
    Ok(())
}
