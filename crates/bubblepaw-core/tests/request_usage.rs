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
    uuid::Uuid::parse_str(&record.request_id)?;
    chrono::DateTime::parse_from_rfc3339(&record.created_at)?;
    store.serving_wipe()?;
    assert_eq!(store.request_usage()?, records);
    drop(engine);
    drop(store);
    assert_eq!(Store::read_request_usage_data_dir(dir.path())?, records);
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

#[test]
fn version_seven_migration_preserves_canonical_rows_and_drops_old_serving_usage() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("legacy.db");
    let connection = Connection::open(&path)?;
    connection.execute_batch(include_str!("fixtures/main-v6-schema.sql"))?;
    connection.execute_batch(include_str!("fixtures/v7-additions.sql"))?;
    let session = serde_json::json!({
        "id":"old-session", "title":"old", "parent_session_id":null,
        "parent_cutoff":null, "created_at":"2026-10-01T00:00:00Z",
        "updated_at":"2026-10-01T00:00:00Z", "read_only":false,
    })
    .to_string();
    let message = serde_json::json!({
        "id":"old-message", "session_id":"old-session", "role":"user",
        "content":"original content", "created_at":"2026-10-01T00:00:00Z",
    })
    .to_string();
    connection.execute(
        "INSERT INTO sessions(id,data) VALUES('old-session',?1)",
        [&session],
    )?;
    connection.execute(
        "INSERT INTO messages(session_id,message_id,data) VALUES('old-session','old-message',?1)",
        [&message],
    )?;
    drop(connection);
    let store = Store::open(&path)?;
    let chat = store.chat_path("old-session")?.unwrap();
    let connection = Connection::open(chat)?;
    assert_eq!(
        connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?,
        11
    );
    assert_eq!(
        connection.query_row("SELECT data FROM messages", [], |row| row
            .get::<_, String>(0))?,
        message
    );
    assert_eq!(
        connection.query_row("SELECT data FROM sessions", [], |row| row
            .get::<_, String>(0))?,
        session
    );
    assert!(
        store.request_usage()?.is_empty(),
        "migration never invents per-request history"
    );
    drop(connection);
    let data_dir = store.data_dir().unwrap().to_path_buf();
    drop(store);
    Connection::open(data_dir.join("serving.db"))?.execute_batch("PRAGMA user_version=1; CREATE TABLE usage(id INTEGER PRIMARY KEY,session_id TEXT,created_at TEXT NOT NULL,data TEXT NOT NULL); INSERT INTO usage VALUES(1,'old-session','2026-10-01T00:00:00Z','{}');")?;
    let store = Store::open_data_dir(&data_dir)?;
    let serving = Connection::open(data_dir.join("serving.db"))?;
    assert_eq!(
        serving.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='usage'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    assert!(store.request_usage()?.is_empty());
    Ok(())
}

#[test]
fn existing_split_version_seven_chat_migrates_on_first_write() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("split migration")?;
    let chat = store.chat_path(&session.id)?.unwrap();
    drop(store);
    Connection::open(&chat)?.execute_batch(
        "DROP TABLE request_usage; PRAGMA user_version=7; PRAGMA wal_checkpoint(TRUNCATE);",
    )?;
    // A report reads v7 facts without migrating or creating invented rows.
    assert!(Store::read_request_usage_data_dir(dir.path())?.is_empty());
    assert_eq!(
        Connection::open(&chat)?
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?,
        7
    );
    let store = Store::open_data_dir(dir.path())?;
    let mut record = RequestUsage::new("provider", "model");
    record.session_id.clone_from(&session.id);
    store.queue_request_usage(record.clone())?;
    store.flush_request_usage(&session.id)?;
    assert_eq!(
        Connection::open(chat)?.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?,
        11
    );
    assert_eq!(store.request_usage()?, vec![record]);
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

#[test]
fn schema_eight_account_backfill_and_read_only_projection_preserve_history() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("schema8")?;
    let mut record = RequestUsage::new("old-route", "model");
    record.session_id.clone_from(&session.id);
    record.usage = usage();
    store.queue_request_usage(record.clone())?;
    store.flush_request_usage(&session.id)?;
    let chat = store.chat_path(&session.id)?.unwrap();
    drop(store);
    Connection::open(&chat)?.execute_batch("ALTER TABLE request_usage DROP COLUMN account; PRAGMA user_version=8; PRAGMA wal_checkpoint(TRUNCATE);")?;
    assert_eq!(
        Store::read_request_usage_data_dir(dir.path())?,
        vec![record.clone()]
    );
    assert_eq!(
        Connection::open(&chat)?
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?,
        8
    );
    let store = Store::open_data_dir(dir.path())?;
    // Opening a chat for a canonical write performs the additive migration.
    store.session(&session.id)?;
    assert_eq!(store.request_usage()?, vec![record.clone()]);
    let connection = Connection::open(&chat)?;
    assert_eq!(
        connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?,
        11
    );
    assert_eq!(
        connection.query_row("SELECT account FROM request_usage", [], |row| row
            .get::<_, String>(0))?,
        record.provider
    );
    Ok(())
}
