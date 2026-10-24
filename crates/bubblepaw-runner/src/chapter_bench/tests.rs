//! Synthetic acceptance through loading, production episodes, all arms and reports.
use super::*;
use crate::replay::ReplayCorpus;
use bubblepaw_core::{Message, MessageOrigin, Role};
use std::{collections::BTreeMap, path::Path};

const ARMS: [&str; 2] = [
    "jev=decider:opencode-decisions/jev-1.13-free",
    "flash-decider=decider:llm/opencode/deepseek-v4.1-flash",
];

#[test]
fn arm_parser_accepts_deciders_and_rejects_retired_or_unknown_kinds() -> Result<()> {
    for route in [
        "native=decider:opencode-decisions/jev-1.13-free",
        "chat=decider:llm/opencode/deepseek-v4.1-flash",
    ] {
        let arm = parse_arm(route)?;
        assert_eq!(arm.kind, "decider");
        assert!(!arm.model.is_empty());
    }
    for route in [
        "retired=marks:opencode/deepseek-v4.1-flash",
        "unknown=other:provider/model",
        "missing=decider:",
        "missing=provider/model",
        "missing-route",
    ] {
        assert!(parse_arm(route).is_err(), "accepted invalid arm {route}");
    }
    Ok(())
}

fn corpus(chat: &str, prefix: &str, transition: bool) -> ReplayCorpus {
    let messages = (0..7)
        .map(|index| {
            let role = if index % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            };
            let mut content = if role == Role::User {
                "Continue the synthetic work.".to_owned()
            } else {
                "work ".repeat(120)
            };
            // Deciders see the current episode.
            // The old chapter also contains the marker, so a stale-summary leak
            // would incorrectly predict another split at the following candidate.
            if transition && matches!(index, 1 | 2) {
                content.push_str(" [boundary]");
            }
            Message {
                id: format!("{prefix}{index}"),
                session_id: format!("source-{chat}"),
                origin: (role == Role::User).then_some(MessageOrigin::Typed),
                role,
                content,
                tool_calls: vec![],
                tool_call_id: None,
                provider_state: None,
                is_error: false,
                created_at: "2026-10-23T00:00:00Z".into(),
            }
        })
        .collect();
    ReplayCorpus {
        chat_id: chat.into(),
        recorded_model: None,
        context_window: None,
        instructions: String::new(),
        messages,
        message_metadata: Value::Null,
        boundary_gold: None,
        relates_gold: vec![],
        provenance: json!({"synthetic":true}),
    }
}

fn summary_config() -> SummaryConfig {
    let mut summary = SummaryConfig {
        threshold_cap: 100,
        ..Default::default()
    };
    summary.lcm.output_reserve_tokens = 0;
    summary.lcm.episodes.source_target_tokens = 80;
    summary.lcm.episodes.source_max_tokens = 200;
    // The shared stage must override both host chapter mechanisms.
    summary.lcm.episodes.decider_enabled = true;
    summary.lcm.episodes.chapter_rollover_tokens = 1;
    summary
}

fn read(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

fn stages(report: &Value) -> BTreeMap<String, String> {
    report["episodes"]["chats"]
        .as_array()
        .unwrap()
        .iter()
        .map(|chat| {
            (
                chat["chat"].as_str().unwrap().to_owned(),
                chat["stage_id"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn assert_report(report: &Value) {
    assert_eq!(report["status"], "completed", "{report}");
    assert_eq!(report["mode"], "scripted");
    assert_eq!(report["errors"], json!([]));
    assert_eq!(report["episodes"]["model"], SUMMARY_MODEL);
    let episodes = report["episodes"]["chats"].as_array().unwrap();
    assert_eq!(episodes.len(), 2);
    for chat in episodes {
        assert_eq!(chat["episodes"], 3);
        assert_eq!(chat["candidates"], 2);
        assert_eq!(chat["uncovered_tail_messages"], 1);
        assert_eq!(chat["usage"]["calls"], 3);
        assert_eq!(chat["usage"]["unknown_cost_calls"], 0);
    }
    assert_eq!(report["episodes"]["totals"]["calls"], 6);
    let arms = report["arms"].as_array().unwrap();
    assert_eq!(arms.len(), 2);
    for (arm, expected) in arms.iter().zip(ARMS) {
        let parsed = parse_arm(expected).unwrap();
        assert_eq!(arm["name"], parsed.name);
        assert_eq!(arm["kind"], parsed.kind);
        assert_eq!(arm["model"], parsed.model);
        assert_eq!(arm["totals"]["errors"], 0);
        assert_eq!(arm["totals"]["cost_usd"], 0.0);
        assert_eq!(arm["totals"]["unknown_cost_calls"], 0);
        assert!(arm["totals"]["input_tokens"].as_u64().unwrap() > 0);
        assert!(arm["totals"]["output_tokens"].as_u64().unwrap() > 0);
        for score in ["exact", "tolerant"] {
            assert_eq!(arm[score]["tp"], 1);
            assert_eq!(arm[score]["fp"], 0);
            assert_eq!(arm[score]["fn"], 0);
            assert_eq!(arm[score]["f1"], 1.0);
        }
        let chats = arm["chats"].as_array().unwrap();
        assert_eq!(chats.len(), 2);
        for chat in chats {
            let expected = if chat["chat"] == "alpha" {
                json!(["a2"])
            } else {
                assert_eq!(chat["chat"], "beta");
                json!([])
            };
            assert_eq!(chat["predicted_ids"], expected);
            assert_eq!(chat["gold_ids"], expected);
            assert_eq!(chat["exact"]["f1"], 1.0);
            assert_eq!(chat["tolerant"]["f1"], 1.0);
        }
    }
}

#[tokio::test]
async fn two_chat_two_arm_benchmark_scores_reports_and_reuses_frozen_episodes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut chats = Vec::new();
    let mut snapshots = Vec::new();
    for (chat, prefix, transition) in [("alpha", "a", true), ("beta", "b", false)] {
        let path = directory.path().join(format!("{chat}.json"));
        let bytes = serde_json::to_vec(&corpus(chat, prefix, transition))?;
        std::fs::write(&path, &bytes)?;
        snapshots.push((path.clone(), bytes));
        chats.push(path);
    }
    let gold = directory.path().join("labels-lead.json");
    let gold_bytes = serde_json::to_vec(&json!({"chats":{
        "alpha":{"chapters":[{"id":"a-first","starts_at":"a0"},{"id":"a-next","starts_at":"a2"}],"ambiguous":[]},
        "beta":{"chapters":[{"id":"b-first","starts_at":"b0"}],"ambiguous":[]}
    }}))?;
    std::fs::write(&gold, &gold_bytes)?;
    let options = BenchOptions {
        chats,
        gold: gold.clone(),
        out: directory.path().join("out"),
        arms: ARMS.into_iter().map(str::to_owned).collect(),
        dry_run: true,
        refresh_episodes: false,
        max_cost_usd: 0.0,
    };
    let first = run(
        options.clone(),
        scripted_registry(&options)?,
        summary_config(),
    )
    .await?;
    assert_report(&first);
    assert_eq!(read(&options.out.join("report.json"))?, first);
    let markdown = std::fs::read_to_string(options.out.join("report.md"))?;
    assert!(markdown.contains("episode generation") && markdown.contains("flash-decider"));
    let first_stages = stages(&first);
    let first_ledger = first["ledger_totals"]["calls"].as_u64().unwrap();
    let arm_calls: u64 = first["arms"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arm| arm["totals"]["calls"].as_u64().unwrap())
        .sum();
    assert_eq!(first["invocation_totals"]["calls"], 6 + arm_calls);

    let resumed = run(
        options.clone(),
        scripted_registry(&options)?,
        summary_config(),
    )
    .await?;
    assert_report(&resumed);
    assert_eq!(stages(&resumed), first_stages);
    assert_eq!(resumed["invocation_totals"]["calls"], arm_calls);
    assert_eq!(resumed["ledger_totals"]["calls"], first_ledger + arm_calls);
    for (chat, stage) in first_stages {
        let frozen = read(&options.out.join("episodes").join(format!("{chat}.json")))?;
        assert_eq!(frozen["complete"], true);
        assert_eq!(frozen["stage_id"], stage);
        assert_eq!(frozen["messages_imported"], 7);
        assert_eq!(frozen["covered_messages"], 6);
    }
    for (path, bytes) in snapshots {
        assert_eq!(std::fs::read(path)?, bytes);
    }
    assert_eq!(std::fs::read(gold)?, gold_bytes);
    Ok(())
}

#[tokio::test]
async fn orchestrator_cost_cap_preserves_the_first_committed_episode_and_reports_incomplete()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let chat = directory.path().join("alpha.json");
    std::fs::write(&chat, serde_json::to_vec(&corpus("alpha", "a", true))?)?;
    let gold = directory.path().join("labels-lead.json");
    std::fs::write(
        &gold,
        serde_json::to_vec(&json!({"chats":{
            "alpha":{"chapters":[{"id":"first","starts_at":"a0"},{"id":"next","starts_at":"a2"}],"ambiguous":[]}
        }}))?,
    )?;
    let options = BenchOptions {
        chats: vec![chat],
        gold,
        out: directory.path().join("out"),
        arms: ARMS.into_iter().map(str::to_owned).collect(),
        // Live accounting charges the priced summary model. The deliberately
        // supplied scripted registry still performs no network requests.
        dry_run: false,
        refresh_episodes: false,
        max_cost_usd: 0.0,
    };
    let report = run(
        options.clone(),
        scripted_registry(&options)?,
        summary_config(),
    )
    .await?;
    assert_eq!(report["status"], "incomplete");
    assert_eq!(report["mode"], "live");
    assert_eq!(report["invocation_totals"]["calls"], 1);
    assert_eq!(report["invocation_totals"]["unknown_cost_calls"], 0);
    assert!(report["invocation_totals"]["cost_usd"].as_f64().unwrap() > 0.0);
    let failures = report["errors"].as_array().unwrap();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0]["stage"], "episodes");
    assert!(
        failures[0]["error"]
            .as_str()
            .unwrap()
            .contains("max-cost-usd")
    );
    assert!(
        report["arms"]
            .as_array()
            .unwrap()
            .iter()
            .all(|arm| arm["totals"]["calls"] == 0 && arm["chats"] == json!([]))
    );
    let frozen = read(&options.out.join("episodes/alpha.json"))?;
    assert_eq!(frozen["complete"], false);
    assert_eq!(frozen["episodes"].as_array().unwrap().len(), 1);
    assert_eq!(frozen["covered_messages"], 2);
    let store = bubblepaw_core::Store::open_data_dir(
        options
            .out
            .join("episodes/stages/alpha")
            .join(frozen["stage_id"].as_str().unwrap())
            .join("store"),
    )?;
    let committed = store.context_episodes(frozen["session_id"].as_str().unwrap())?;
    assert_eq!(committed.len(), 1);
    assert_eq!(committed[0].last_message_id, "a1");
    assert_eq!(read(&options.out.join("report.json"))?, report);
    Ok(())
}
