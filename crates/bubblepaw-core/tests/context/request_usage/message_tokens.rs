use super::*;

#[tokio::test]
async fn physical_attempts_keep_their_routes_and_replace_logical_fallback() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("physical")?;
    let engine = harness(store.clone(), false, true, true);
    let run = engine
        .prompt(&session.id, "hello", RunOptions::default())
        .await?;
    let records = store.request_usage()?;
    assert_eq!(records.len(), 2);
    assert!(
        records
            .iter()
            .all(|record| record.run_id.as_deref() == Some(run.id.as_str()))
    );
    assert!(
        records
            .iter()
            .any(|record| record.provider == "first" && record.model == "m1" && record.failed)
    );
    assert!(
        records
            .iter()
            .any(|record| record.provider == "second" && record.model == "m2" && !record.failed)
    );
    assert_eq!(
        run.usage,
        Some(usage()),
        "run aggregates retain their existing contract"
    );
    let contributions = store.reported_message_tokens(&session.id, 0..=i64::MAX)?;
    assert_eq!(contributions[1].tokens, Some(usage().output_tokens));
    let connection = Connection::open(store.chat_path(&session.id)?.unwrap())?;
    assert_eq!(
        connection.query_row(
            "SELECT COUNT(*) FROM request_usage WHERE failed=1 AND message_seq IS NOT NULL",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    assert_eq!(
        connection.query_row(
            "SELECT COUNT(*) FROM request_usage WHERE failed=0 AND message_seq IS NOT NULL",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );
    Ok(())
}

struct TokenModel {
    attempts: Vec<(bool, u64, bool)>,
    reported: Option<Usage>,
    last: Option<bubblepaw_core::LastRequestUsage>,
}

#[async_trait]
impl Provider for TokenModel {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        for (failed, output, reported) in &self.attempts {
            let mut record = RequestUsage::new("physical", "model");
            record.failed = *failed;
            record.usage.output_tokens = *output;
            assert!(bubblepaw_core::record_request_usage_with_output_presence(
                record, *reported
            ));
            if *failed {
                // Simulate the registry rejecting all contributions of the failed logical attempt.
                assert!(bubblepaw_core::discard_request_usage_since(0));
            }
        }
        Ok(ModelResponse {
            text: "joined reply".into(),
            usage: self.reported.clone(),
            last_request_usage: self.last.clone(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn continuations_sum_only_successful_final_attempt_and_keep_metadata_reads_read_only()
-> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("joined physical output")?;
    let engine = Harness::new(
        store.clone(),
        Arc::new(TokenModel {
            // A prior route produced a partial reply before its continuation failed.
            attempts: vec![
                (false, 11, true),
                (true, 99, true),
                (false, 5, true),
                (false, 7, true),
            ],
            reported: Some(Usage {
                output_tokens: 12,
                ..Default::default()
            }),
            last: Some(bubblepaw_core::LastRequestUsage {
                input_tokens: None,
                output_tokens: Some(7),
            }),
        }),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    engine
        .prompt(&session.id, "hello", RunOptions::default())
        .await?;
    let messages = store.messages(&session.id)?;
    let assistant_seq = store.message_seq(&session.id, &messages[1].id)?;
    let facts = store.reported_message_tokens(&session.id, assistant_seq..=assistant_seq)?;
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].message_id, messages[1].id);
    assert_eq!(facts[0].tokens, Some(12));
    let connection = Connection::open(store.chat_path(&session.id)?.unwrap())?;
    assert_eq!(
        connection.query_row(
            "SELECT COUNT(*) FROM request_usage WHERE message_seq IS NOT NULL",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        2
    );

    // Missing reports require no source-body hydration, even for malformed bodies.
    let tool: bubblepaw_core::Message = serde_json::from_value(serde_json::json!({
        "id":"tool", "session_id":session.id, "role":"tool", "content":"result text",
        "tool_call_id":"call", "created_at":"2026-10-27T00:00:00Z"
    }))?;
    store.append_batch(std::slice::from_ref(&tool))?;
    connection.execute(
        "UPDATE messages SET data='unreadable body' WHERE session_id=?1",
        [&session.id],
    )?;
    let before = store.storage_stats()?;
    let facts = store.reported_message_tokens(&session.id, 0..=i64::MAX)?;
    assert_eq!(facts.len(), 3);
    assert_eq!(facts[0].message_id, messages[0].id);
    assert_eq!(facts[0].tokens, None);
    assert_eq!(facts[1].message_id, messages[1].id);
    assert_eq!(facts[1].tokens, Some(12));
    assert_eq!(facts[2].message_id, tool.id);
    assert_eq!(facts[2].tokens, None);
    let after = store.storage_stats()?;
    assert_eq!(after.chat_transaction_count, before.chat_transaction_count);
    assert_eq!(
        after.serving_transaction_count,
        before.serving_transaction_count
    );
    assert_eq!(after.wal_bytes, before.wal_bytes);
    Ok(())
}

#[tokio::test]
async fn actual_reported_zero_is_distinct_from_missing_usage() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    for reported in [None, Some(Usage::default())] {
        let session = store.create_session("zero output")?;
        let engine = Harness::new(
            store.clone(),
            Arc::new(TokenModel {
                attempts: Vec::new(),
                reported: reported.clone(),
                last: None,
            }),
            ToolRegistry::new(),
            HarnessConfig::default(),
        );
        engine
            .prompt(&session.id, "hello", RunOptions::default())
            .await?;
        let facts = store.reported_message_tokens(&session.id, 0..=i64::MAX)?;
        if reported.is_some() {
            assert_eq!(facts[1].tokens, Some(0));
        } else {
            assert_eq!(facts[1].tokens, None);
        }
    }
    Ok(())
}

#[tokio::test]
async fn failed_usage_totals_do_not_make_unreported_success_output_reported() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    for last_output in [None, Some(0)] {
        let session = store.create_session("fallback output presence")?;
        let engine = Harness::new(
            store.clone(),
            Arc::new(TokenModel {
                attempts: vec![(true, 99, true), (false, 0, last_output.is_some())],
                // Registry totals include failed billed output even when final usage is absent.
                reported: Some(Usage {
                    output_tokens: 99,
                    ..Default::default()
                }),
                last: Some(bubblepaw_core::LastRequestUsage {
                    input_tokens: None,
                    output_tokens: last_output,
                }),
            }),
            ToolRegistry::new(),
            HarnessConfig::default(),
        );
        engine
            .prompt(&session.id, "hello", RunOptions::default())
            .await?;
        let facts = store.reported_message_tokens(&session.id, 0..=i64::MAX)?;
        if last_output.is_some() {
            assert_eq!(facts[1].tokens, Some(0));
        } else {
            assert_eq!(facts[1].tokens, None);
        }
    }
    Ok(())
}

#[tokio::test]
async fn link_failure_rolls_back_reply_and_session_deletion_clears_stale_links() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let root = store.create_session("root")?;
    let session = store.create_session_with_parent("child", Default::default(), Some(&root.id))?;
    let connection = Connection::open(store.chat_path(&session.id)?.unwrap())?;
    connection.execute_batch("CREATE TRIGGER reject_link BEFORE UPDATE OF message_seq ON request_usage BEGIN SELECT RAISE(ABORT,'link failure'); END;")?;
    let engine = harness(store.clone(), false, true, false);
    let failed = engine
        .prompt(&session.id, "first", RunOptions::default())
        .await?;
    assert_eq!(failed.status, "failed");
    assert!(store.request_usage()?.is_empty());
    assert!(
        store
            .messages(&session.id)?
            .iter()
            .all(|message| message.role != Role::Assistant)
    );
    connection.execute_batch("DROP TRIGGER reject_link;")?;
    // The failed terminal transaction also rolled back the run's settled state.
    // Reopening performs the ordinary interrupted-run recovery before another prompt.
    drop(engine);
    drop(connection);
    drop(store);
    let store = Store::open_data_dir(dir.path())?;
    let engine = harness(store.clone(), false, true, false);
    let connection = Connection::open(store.chat_path(&session.id)?.unwrap())?;
    let completed = engine
        .prompt(&session.id, "second", RunOptions::default())
        .await?;
    assert_eq!(completed.status, "completed");
    assert_eq!(
        connection.query_row(
            "SELECT COUNT(*) FROM request_usage WHERE message_seq IS NOT NULL",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );
    let records = store.request_usage()?;
    store.delete_session(&session.id)?;
    assert_eq!(store.request_usage()?, records);
    assert_eq!(
        connection.query_row(
            "SELECT COUNT(*) FROM request_usage WHERE message_seq IS NOT NULL",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    Ok(())
}

#[derive(Clone, Copy)]
enum AttributionAction {
    Reported(u64),
    Unknown,
    Discard,
}

struct AttributionModel(Vec<AttributionAction>);

#[async_trait]
impl Provider for AttributionModel {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let (mut total, mut last_output) = (0, None);
        for action in &self.0 {
            if matches!(action, AttributionAction::Discard) {
                // A logical attempt may fail after recording physically successful rounds.
                assert!(bubblepaw_core::discard_request_usage_since(0));
                total = 0;
                last_output = None;
                continue;
            }
            let mut record = RequestUsage::new("physical", "model");
            let reported = match action {
                AttributionAction::Reported(tokens) => {
                    record.usage.output_tokens = *tokens;
                    total += tokens;
                    last_output = Some(*tokens);
                    true
                }
                AttributionAction::Unknown => {
                    last_output = None;
                    false
                }
                AttributionAction::Discard => unreachable!(),
            };
            assert!(bubblepaw_core::record_request_usage_with_output_presence(
                record, reported
            ));
        }
        Ok(ModelResponse {
            text: "joined reply".into(),
            usage: Some(Usage {
                output_tokens: total,
                ..Default::default()
            }),
            last_request_usage: Some(bubblepaw_core::LastRequestUsage {
                input_tokens: None,
                output_tokens: last_output,
            }),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn attribution_requires_every_contributing_round_and_excludes_discarded_successes()
-> Result<()> {
    use AttributionAction::{Discard, Reported, Unknown};
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    for (actions, expected, linked) in [
        (vec![Reported(11), Discard, Reported(7)], Some(7), 1),
        (vec![Unknown, Reported(7)], None, 0),
        (vec![Reported(0), Reported(7)], Some(7), 2),
        (vec![Reported(0)], Some(0), 1),
        (vec![Reported(11), Discard, Unknown, Reported(7)], None, 0),
        (vec![Unknown, Discard, Reported(7)], Some(7), 1),
    ] {
        let session = store.create_session("physical attribution")?;
        let physical_count = actions
            .iter()
            .filter(|action| !matches!(action, Discard))
            .count();
        let engine = Harness::new(
            store.clone(),
            Arc::new(AttributionModel(actions)),
            ToolRegistry::new(),
            HarnessConfig::default(),
        );
        let before = store.storage_stats()?.chat_transaction_count;
        let run = engine
            .prompt(&session.id, "hello", RunOptions::default())
            .await?;
        assert_eq!(run.status, "completed");
        assert_eq!(store.storage_stats()?.chat_transaction_count - before, 2);
        let facts = store.reported_message_tokens(&session.id, 0..=i64::MAX)?;
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[1].tokens, expected);
        let connection = Connection::open(store.chat_path(&session.id)?.unwrap())?;
        assert_eq!(connection.query_row("SELECT COUNT(*) FROM request_usage WHERE session_id=?1 AND message_seq IS NOT NULL", [&session.id], |row| row.get::<_, i64>(0))?, linked);
        let billing: Vec<_> = store
            .request_usage()?
            .into_iter()
            .filter(|row| row.session_id == session.id)
            .collect();
        assert_eq!(billing.len(), physical_count);
        assert!(
            billing.iter().all(|row| !row.failed),
            "discarding a logical attempt must not rewrite physical billing outcomes"
        );
        let json = serde_json::to_value(&billing[0])?;
        assert!(json.get("output_reported").is_none());
        assert!(json.get("discarded").is_none());
    }
    Ok(())
}

#[tokio::test]
async fn public_capture_keeps_billing_rows_and_nested_discard_markers_are_isolated() -> Result<()> {
    use bubblepaw_core::{
        capture_request_usage, discard_request_usage_since,
        record_request_usage_with_output_presence,
    };
    assert!(!discard_request_usage_since(0));
    let (_, outer) = capture_request_usage("outer", None, RequestKind::Main, async {
        assert!(record_request_usage_with_output_presence(
            RequestUsage::new("p", "m"),
            false
        ));
        let (_, inner) = capture_request_usage("inner", None, RequestKind::Main, async {
            assert_eq!(request_usage_count(), Some(0));
            assert!(record_request_usage(RequestUsage::new("p", "m")));
            assert!(discard_request_usage_since(0));
            assert!(!discard_request_usage_since(2));
        })
        .await;
        assert_eq!(inner.len(), 1);
        assert_eq!(inner[0].session_id, "inner");
        assert_eq!(request_usage_count(), Some(1));
        assert!(discard_request_usage_since(0));
        assert!(record_request_usage(RequestUsage::new("p", "m")));
    })
    .await;
    assert_eq!(outer.len(), 2);
    assert!(
        outer
            .iter()
            .all(|record| record.session_id == "outer" && !record.failed)
    );
    Ok(())
}
