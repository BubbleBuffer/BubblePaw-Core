//! Subject transitions wait for a complete episode.
use super::*;

#[tokio::test]
async fn replay_waits_for_a_complete_episode_before_accepting_a_subject_transition() -> Result<()> {
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
        boundary_gold: Some(json!({"source":"fixture","gold":["r3"],"ambiguous":[],"chapters":[]})),
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
