//! Import batches and worker gates.
use super::*;

#[test]
fn replay_import_batch_rolls_back_all_records_and_only_counts_commits() -> Result<()> {
    let mut db = Store::in_memory()?;
    let session = db.create_session("batch rollback")?;
    let mut batch = ReplayImportBatch::default();
    let mut message = performance_message(0, Role::User);
    message.session_id = session.id.clone();
    batch.push(&message)?;
    let mut invalid = message.clone();
    invalid.session_id = "missing-session".into();
    batch.push(&invalid)?;
    assert!(batch.flush(&mut db).is_err());
    assert!(db.messages(&session.id)?.is_empty());
    assert_eq!(batch.committed_messages, 0);
    assert_eq!(batch.committed_transactions, 0);
    assert_eq!(
        batch.records.len(),
        2,
        "Failed batch remains available for diagnostics"
    );
    batch.records.pop();
    batch.flush(&mut db)?;
    assert_eq!(batch.committed_messages, 1);
    assert_eq!(batch.committed_transactions, 1);
    assert!(batch.records.is_empty());
    batch.flush(&mut db)?;
    assert_eq!(
        batch.committed_transactions, 1,
        "Empty flush is not a write transaction"
    );
    Ok(())
}

#[tokio::test]
async fn replay_batch_and_worker_gates_preserve_both_strategy_contracts() -> Result<()> {
    for malformed in [false, true] {
        let mut messages = Vec::new();
        for turn in 0..4 {
            let mut user = performance_message(messages.len(), Role::User);
            user.content = if turn == 1 {
                "[new subject] Switch to task beta".into()
            } else {
                format!("Continue task {turn}")
            };
            messages.push(user);
            let mut assistant = performance_message(messages.len(), Role::Assistant);
            assistant.content = "transient ".repeat(36_000);
            if turn == 0 {
                assistant.tool_calls = vec![
                    bubblepaw_core::ToolCall {
                        id: "fixture-parallel-a".into(),
                        name: "recorded_only".into(),
                        arguments: json!({"path":"a"}),
                    },
                    bubblepaw_core::ToolCall {
                        id: "fixture-parallel-b".into(),
                        name: "recorded_only".into(),
                        arguments: json!({"path":"b"}),
                    },
                ];
            }
            messages.push(assistant);
            if turn == 0 {
                for call in ["fixture-parallel-a", "fixture-parallel-b"] {
                    let mut tool = performance_message(messages.len(), Role::Tool);
                    tool.tool_call_id = Some(call.into());
                    tool.content = "Recorded result; never execute this tool. ".repeat(500);
                    messages.push(tool);
                }
            }
        }
        if malformed {
            let mut assistant = performance_message(messages.len(), Role::Assistant);
            assistant.tool_calls.push(bubblepaw_core::ToolCall {
                id: "fixture-unresolved".into(),
                name: "recorded_only".into(),
                arguments: json!({}),
            });
            messages.push(assistant);
            messages.push(performance_message(messages.len(), Role::User));
        }
        let corpus = ReplayCorpus {
            chat_id: "performance-oracle".into(),
            recorded_model: None,
            context_window: None,
            instructions: "Keep fixture facts".into(),
            messages: messages.clone(),
            message_metadata: json!([]),
            boundary_gold: None,
            relates_gold: vec![],
            provenance: json!({"fixture":true}),
        };
        let mut normalized_corpus = corpus.clone();
        normalize_archive(&mut normalized_corpus)?;
        let messages = normalized_corpus.messages;
        let canonical_ids: HashSet<_> = messages.iter().map(|message| message.id.clone()).collect();
        for strategy in [ContextStrategy::Lcm, ContextStrategy::Classical] {
            let mut contracts = Vec::new();
            let mut commits = Vec::new();
            for execution in [ReplayExecution::PerMessage, ReplayExecution::Optimized] {
                let directory = tempfile::tempdir()?;
                let options = ReplayOptions {
                    prices: example_prices(),
                    agents: Default::default(),
                    chat: "fixture".into(),
                    out: directory.path().into(),
                    threshold: Some(80_000),
                    dry_run: true,
                    context_strategy: strategy,
                    probes: None,
                    checkpoints_at_turns: vec![1, 2, 3, 4],
                    probe_plan: None,
                    plan_probes: false,
                    capture_turn_frontiers: true,
                    probe_limit: None,
                    max_cost_usd: None,
                };
                let prepared_trace = Mutex::new(Vec::new());
                let mut report = run_corpus_inner(
                    corpus.clone(),
                    options,
                    EpisodeModelConfig::default(),
                    ProviderEndpoints::default(),
                    default_replay_summary_config(),
                    execution,
                    Some(&prepared_trace),
                )
                .await?;
                let store = Store::open_data_dir(directory.path().join("store"))?;
                let session = report["session_id"].as_str().unwrap().to_owned();
                let archived = store.messages(&session)?;
                assert_eq!(archived.len(), messages.len());
                for (expected, actual) in messages.iter().zip(archived) {
                    let mut actual = actual;
                    actual.session_id = expected.session_id.clone();
                    assert_eq!(
                        serde_json::to_value(actual)?,
                        serde_json::to_value(expected)?,
                        "Canonical history must retain exact IDs, content, tools and original timestamps"
                    );
                }
                assert!(store.pending_effects(&session)?.is_empty());
                report["prepared_requests"] =
                    serde_json::to_value(&*prepared_trace.lock().unwrap())?;
                assert!(
                    !report["prepared_requests"].as_array().unwrap().is_empty(),
                    "Capture every checkpoint/compaction request, not only the final view"
                );
                report["durable_serving_view"] =
                    serde_json::to_value(store.context_view(&session)?)?;
                commits.push(
                    report["storage_transaction_basis"]["canonical_import_transactions"]
                        .as_u64()
                        .unwrap(),
                );
                assert_eq!(report["status"], "complete");
                assert_eq!(
                    report["archive_unknown_outcome_markers"],
                    usize::from(malformed)
                );
                assert_eq!(report["source_messages_replayed"], corpus.messages.len());
                contracts.push(normalized_replay_contract(
                    report,
                    &canonical_ids,
                    &messages,
                ));
            }
            assert_eq!(
                contracts[0], contracts[1],
                "strategy {strategy:?}, malformed {malformed}"
            );
            assert!(
                commits[1] < commits[0],
                "Batching must reduce actual canonical commits"
            );
        }
    }
    Ok(())
}

#[test]
fn replay_archive_id_index_accelerates_the_existing_store_query() -> Result<()> {
    let db = rusqlite::Connection::open_in_memory()?;
    db.execute_batch("CREATE TABLE messages(seq INTEGER PRIMARY KEY,session_id TEXT,data TEXT); CREATE INDEX messages_session ON messages(session_id,seq); CREATE INDEX replay_message_id ON messages(session_id,json_extract(data,'$.id'));")?;
    let plan = db.query_row("EXPLAIN QUERY PLAN SELECT seq FROM messages WHERE session_id=?1 AND json_extract(data,'$.id')=?2",
        ["fixture", "gate-0"], |row| row.get::<_, String>(3))?;
    assert!(
        plan.contains("replay_message_id"),
        "ID lookup must use the expression index: {plan}"
    );
    Ok(())
}
