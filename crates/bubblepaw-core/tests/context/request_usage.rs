use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider,
    ProviderError, ProviderErrorKind, RequestKind, RequestUsage, Role, RunOptions, Store,
    TextCallback, ToolRegistry, Usage, capture_request_usage_with_store, record_request_usage,
    request_usage_count, with_request_usage_account, with_request_usage_provider,
};
use rusqlite::Connection;
use std::sync::Arc;

fn usage() -> Usage {
    Usage {
        input_tokens: 100,
        cache_read_tokens: 60,
        cache_creation_tokens: 10,
        output_tokens: 12,
        reasoning_tokens: Some(5),
    }
}

struct Model {
    failed: bool,
    report_usage: bool,
    physical: bool,
}
#[async_trait]
impl Provider for Model {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        if self.physical {
            for (provider, model, failed) in [("first", "m1", true), ("second", "m2", false)] {
                let mut record = RequestUsage::new(provider, model);
                record.usage = usage();
                record.failed = failed;
                assert!(record_request_usage(record));
            }
        }
        if self.failed {
            let error = ProviderError::new(ProviderErrorKind::Fatal, "scripted failure");
            return Err(if self.report_usage {
                error.with_usage(usage())
            } else {
                error
            }
            .into());
        }
        Ok(ModelResponse {
            text: "answer".into(),
            usage: self.report_usage.then(usage),
            ..Default::default()
        })
    }
}

fn harness(store: Store, failed: bool, report_usage: bool, physical: bool) -> Harness {
    Harness::new(
        store,
        Arc::new(Model {
            failed,
            report_usage,
            physical,
        }),
        ToolRegistry::new(),
        HarnessConfig {
            model: "scripted/model".into(),
            ..Default::default()
        },
    )
}

#[tokio::test]
async fn final_reply_usage_is_durable_without_an_extra_transaction() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("usage")?;
    let engine = harness(store.clone(), false, true, false);
    let before = store.storage_stats()?;
    let run = engine
        .prompt(&session.id, "hello", RunOptions::default())
        .await?;
    let after = store.storage_stats()?;
    assert_eq!(run.status, "completed");
    assert_eq!(run.usage, Some(usage()));
    // The pre-usage ordinary final turn has start and final canonical commits.
    assert_eq!(
        after.chat_transaction_count - before.chat_transaction_count,
        2
    );
    assert_eq!(
        after.serving_transaction_count - before.serving_transaction_count,
        1
    );
    assert_eq!(
        after.catalog_transaction_count - before.catalog_transaction_count,
        0
    );
    let records = store.request_usage()?;
    println!(
        "ordinary request usage: rows={}, chat_tx={}, serving_tx={}, catalog_tx={}, database_bytes={}->{}, wal_bytes={}->{}, chat_bytes={}->{}, serving_bytes={}->{}",
        records.len(),
        after.chat_transaction_count - before.chat_transaction_count,
        after.serving_transaction_count - before.serving_transaction_count,
        after.catalog_transaction_count - before.catalog_transaction_count,
        before.database_bytes,
        after.database_bytes,
        before.wal_bytes,
        after.wal_bytes,
        before.chat_bytes,
        after.chat_bytes,
        before.serving_bytes,
        after.serving_bytes
    );
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.session_id, session.id);
    assert_eq!(record.run_id.as_deref(), Some(run.id.as_str()));
    assert_eq!(record.provider, "scripted");
    assert_eq!(record.account, "scripted");
    assert_eq!(record.model, "model");
    assert_eq!(record.kind, RequestKind::Main);
    assert_eq!(record.usage, usage());
    assert!(!record.failed);
    let before_read = store.storage_stats()?.chat_transaction_count;
    let contributions = store.reported_message_tokens(&session.id, 0..=i64::MAX)?;
    assert_eq!(contributions.len(), 2);
    assert_eq!(contributions[0].tokens, None);
    assert_eq!(contributions[1].tokens, Some(usage().output_tokens));
    assert_eq!(store.storage_stats()?.chat_transaction_count, before_read);
    uuid::Uuid::parse_str(&record.request_id)?;
    chrono::DateTime::parse_from_rfc3339(&record.created_at)?;
    store.serving_wipe()?;
    assert_eq!(store.request_usage()?, records);
    assert_eq!(
        store.reported_message_tokens(&session.id, 0..=i64::MAX)?,
        contributions
    );
    drop(engine);
    drop(store);
    assert_eq!(Store::read_request_usage_data_dir(dir.path())?, records);
    let store = Store::open_data_dir(dir.path())?;
    assert_eq!(
        store.reported_message_tokens(&session.id, 0..=i64::MAX)?,
        contributions
    );
    Ok(())
}

#[tokio::test]
async fn failed_requests_keep_partial_or_zero_usage_without_assistant_messages() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    for report_usage in [true, false] {
        let session = store.create_session("failure")?;
        let engine = harness(store.clone(), true, report_usage, false);
        let run = engine
            .prompt(&session.id, "hello", RunOptions::default())
            .await?;
        assert_eq!(run.status, "failed");
        assert_eq!(run.usage, report_usage.then(usage));
        assert!(
            store
                .messages(&session.id)?
                .iter()
                .all(|message| message.role != Role::Assistant)
        );
        let records: Vec<_> = store
            .request_usage()?
            .into_iter()
            .filter(|record| record.session_id == session.id)
            .collect();
        assert_eq!(records.len(), 1);
        assert!(records[0].failed);
        assert_eq!(
            records[0].usage,
            if report_usage {
                usage()
            } else {
                Usage::default()
            }
        );
    }
    Ok(())
}

#[tokio::test]
async fn rejecting_usage_rolls_back_the_assistant_in_the_same_transaction() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("atomic")?;
    Connection::open(store.chat_path(&session.id)?.unwrap())?.execute_batch(
        "CREATE TRIGGER reject_usage BEFORE INSERT ON request_usage BEGIN SELECT RAISE(ABORT,'synthetic usage failure'); END;")?;
    let engine = harness(store.clone(), false, true, false);
    let run = engine
        .prompt(&session.id, "hello", RunOptions::default())
        .await?;
    assert_eq!(run.status, "failed");
    assert!(store.request_usage()?.is_empty());
    assert!(
        store
            .messages(&session.id)?
            .iter()
            .all(|message| message.role != Role::Assistant)
    );
    Ok(())
}

#[test]
fn auxiliary_usage_piggybacks_rolls_back_and_flushes_only_when_needed() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("auxiliary")?;
    let mut record = RequestUsage::new("provider", "summary");
    record.session_id.clone_from(&session.id);
    record.kind = RequestKind::SummaryFork;
    record.usage = usage();
    let before = store.storage_stats()?.chat_transaction_count;
    store.queue_request_usage(record.clone())?;
    assert!(store.request_usage()?.is_empty());
    Connection::open(store.chat_path(&session.id)?.unwrap())?.execute_batch(
        "CREATE TRIGGER reject_state BEFORE INSERT ON host_state BEGIN SELECT RAISE(ABORT,'synthetic state failure'); END;")?;
    assert!(
        store
            .update_host_state(&session.id, "", "test", |_| Ok(
                serde_json::json!({"state":1})
            ))
            .is_err()
    );
    assert!(store.request_usage()?.is_empty());
    Connection::open(store.chat_path(&session.id)?.unwrap())?
        .execute_batch("DROP TRIGGER reject_state")?;
    store.update_host_state(&session.id, "", "test", |_| {
        Ok(serde_json::json!({"state":1}))
    })?;
    assert_eq!(store.request_usage()?, vec![record.clone()]);
    assert_eq!(store.storage_stats()?.chat_transaction_count - before, 1);
    let before = store.storage_stats()?.chat_transaction_count;
    store.flush_request_usage(&session.id)?;
    assert_eq!(store.storage_stats()?.chat_transaction_count, before);
    store.queue_request_usage(record)?; // idempotent completion retry
    store.flush_request_usage(&session.id)?;
    assert_eq!(store.request_usage()?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn capture_supplies_scope_and_preserves_completed_attempts_when_dropped() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("capture")?;
    let mut capture = Box::pin(capture_request_usage_with_store(
        &store,
        &session.id,
        None,
        RequestKind::Decider,
        async {
            with_request_usage_provider("configured-route", async {
                assert!(record_request_usage(RequestUsage::new(
                    "protocol",
                    "actual-model"
                )));
            })
            .await;
            assert_eq!(request_usage_count(), Some(1));
            std::future::pending::<()>().await;
        },
    ));
    tokio::select! {
        _ = &mut capture => panic!("capture must remain pending"),
        _ = tokio::task::yield_now() => {},
    }
    // Dropping the scoped future synchronously preserves completed attempts.
    drop(capture);
    let records = store.request_usage()?;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, RequestKind::Decider);
    assert_eq!(records[0].provider, "configured-route");
    assert_eq!(records[0].model, "actual-model");
    assert_eq!(records[0].session_id, session.id);
    assert_eq!(records[0].run_id, None);
    Ok(())
}

#[tokio::test]
async fn child_cleanup_and_forks_preserve_original_billed_rows_and_archive_reads() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let root = store.create_session("root")?;
    let child = store.create_session_with_parent("child", Default::default(), Some(&root.id))?;
    let engine = harness(store.clone(), false, true, false);
    engine
        .prompt(&child.id, "hello", RunOptions::default())
        .await?;
    let original = store.request_usage()?;
    assert_eq!(original.len(), 1);
    assert_eq!(original[0].session_id, child.id);
    let fork = store.fork(&child.id, "fork")?;
    assert_eq!(
        store.request_usage()?,
        original,
        "forks do not duplicate billed history"
    );
    store.delete_session(&fork.id)?;
    store.delete_session_preserving_effects(&child.id)?;
    assert_eq!(
        store.request_usage()?,
        original,
        "worker cleanup retains billed usage"
    );
    drop(engine);
    let chat = store.chat_path(&root.id)?.unwrap();
    assert!(store.archive_chat(&root.id)?);
    assert!(!chat.exists());
    let archive = std::path::PathBuf::from(format!("{}.zst", chat.display()));
    let bytes = std::fs::read(&archive)?;
    assert_eq!(Store::read_request_usage_data_dir(dir.path())?, original);
    assert_eq!(std::fs::read(&archive)?, bytes);
    assert!(!chat.exists(), "usage queries do not restore archives");
    Ok(())
}

#[tokio::test]
async fn accounts_default_to_provider_and_scopes_are_nested_and_task_local() -> Result<()> {
    assert_eq!(RequestUsage::new("p", "m").account, "p");
    let record = RequestUsage::new("legacy", "m");
    let mut json = serde_json::to_value(&record)?;
    json.as_object_mut().unwrap().remove("account");
    assert_eq!(
        serde_json::from_value::<RequestUsage>(json)?.account,
        "legacy"
    );
    let (_, records) = bubblepaw_core::capture_request_usage(
        "session",
        None,
        RequestKind::Main,
        with_request_usage_provider("configured-route", async {
            assert!(record_request_usage(RequestUsage::new("adapter", "m")));
            with_request_usage_account("work", async {
                assert_eq!(RequestUsage::new("adapter", "m").account, "work");
                assert!(record_request_usage(RequestUsage::new("adapter", "m")));
                with_request_usage_account("personal", async {
                    assert!(record_request_usage(RequestUsage::new("adapter", "m")));
                })
                .await;
                assert_eq!(RequestUsage::new("adapter", "m").account, "work");
                let spawned = tokio::spawn(async { RequestUsage::new("isolated", "m").account })
                    .await
                    .unwrap();
                assert_eq!(spawned, "isolated");
            })
            .await;
            assert!(record_request_usage(RequestUsage::new("adapter", "m")));
        }),
    )
    .await;
    assert_eq!(
        records
            .iter()
            .map(|r| r.account.as_str())
            .collect::<Vec<_>>(),
        vec!["configured-route", "work", "personal", "configured-route"]
    );
    assert!(records.iter().all(|r| r.provider == "configured-route"));
    Ok(())
}

#[path = "request_usage/message_tokens.rs"]
mod message_tokens;
