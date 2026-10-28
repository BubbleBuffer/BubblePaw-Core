use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, EventKind, Harness, HarnessConfig, ModelRequest, ModelResponse, Observation,
    ObservationMode, ObservationRecord, ObservationSink, Provider, QuotaReading, QuotaSource,
    QuotaWait, QuotaWaitChanged, Run, RunOptions, Store, StoreOptions, TextCallback, ToolRegistry,
    clear_current_quota_wait, set_quota_wait,
};
use rusqlite::{Connection, params};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

const DATE: &str = "2026-10-20T19:00:00Z";
const RESET: &str = "2026-10-21T00:00:00Z";
fn reading() -> QuotaReading {
    QuotaReading {
        provider: "p".into(),
        account: "work".into(),
        window: "weekly".into(),
        used_percent: 80.0,
        resets_at: Some(RESET.into()),
        observed_at: DATE.into(),
        source: QuotaSource::ResponseHeaders,
        balance: None,
    }
}

#[test]
fn quota_changes_are_shared_durable_and_identical_observations_never_write() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("quota")?;
    let before = store.storage_stats()?;
    let first = reading();
    assert!(store.record_quota(&first)?);
    let after = store.storage_stats()?;
    assert_eq!(
        after.catalog_transaction_count - before.catalog_transaction_count,
        1
    );
    assert_eq!(after.chat_transaction_count, before.chat_transaction_count);
    assert_eq!(
        after.serving_transaction_count,
        before.serving_transaction_count
    );
    let mut repeated = first.clone();
    repeated.observed_at = "2026-10-20T20:00:00Z".into();
    for _ in 0..25 {
        assert!(!store.record_quota(&repeated)?);
    }
    let same = store.storage_stats()?;
    assert_eq!(same.transaction_count, after.transaction_count);
    assert_eq!(same.wal_bytes, after.wal_bytes);
    assert_eq!(store.latest_quota()?, vec![first.clone()]);
    let mut changed = repeated;
    changed.used_percent = 95.0;
    assert!(store.record_quota(&changed)?);
    assert!(
        !store.record_quota(&first)?,
        "late observation cannot regress latest"
    );
    let mut other = changed.clone();
    other.provider = "other-provider".into();
    other.source = QuotaSource::SdkEvent;
    other.resets_at = Some("2026-10-21T00:00:00+00:00".into());
    assert!(
        !store.record_quota(&other)?,
        "provider aliases and provenance do not change the account meter"
    );
    other.account = "personal".into();
    assert!(store.record_quota(&other)?);
    let history = vec![first, changed.clone(), other];
    assert_eq!(store.quota_history()?, history);
    store.serving_wipe()?;
    store.catalog_rebuild()?;
    assert!(store.archive_chat(&session.id)?);
    assert_eq!(store.quota_history()?, history);
    store.delete_session(&session.id)?;
    assert_eq!(store.quota_history()?, history);
    // Read-only CLI access works while an owner still holds this store.
    assert_eq!(Store::read_quota_data_dir(dir.path(), false)?, history);
    println!(
        "quota: first catalog tx=1; repeat tx=0 WAL delta={}; catalog bytes={} -> {}",
        same.wal_bytes - after.wal_bytes,
        before.catalog_bytes,
        same.catalog_bytes
    );
    drop(store);
    let reopened = Store::open_data_dir(dir.path())?;
    assert_eq!(reopened.quota_history()?, history);
    assert_eq!(reopened.latest_quota()?.len(), 2);
    Ok(())
}

#[test]
fn balances_ride_change_rows_and_never_write_alone() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let before = store.storage_stats()?;
    for index in 0..10 {
        let mut credits = reading();
        credits.window = "credits".into();
        credits.used_percent = 0.0;
        credits.resets_at = None;
        credits.observed_at = format!("2026-10-21T00:00:{index:02}Z");
        credits.balance = Some(format!("{}", 100 - index));
        // Only the first observation changes the meter; the rest carry no write.
        assert_eq!(store.record_quota(&credits)?, index == 0);
    }
    let history = store.quota_history()?;
    assert_eq!(history.len(), 1);
    // The row keeps the balance supplied with its change, stamped with its own time.
    assert_eq!(history[0].balance.as_deref(), Some("100"));
    assert_eq!(history[0].observed_at, "2026-10-21T00:00:00Z");
    assert_eq!(
        store.storage_stats()?.catalog_transaction_count - before.catalog_transaction_count,
        1
    );
    drop(store);
    assert_eq!(Store::read_quota_data_dir(dir.path(), false)?, history);
    Ok(())
}

#[test]
fn changed_window_without_balance_carries_last_stored_account_balance() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let mut credits = reading();
    credits.window = "credits".into();
    credits.balance = Some("12.34".into());
    store.record_quota(&credits)?;
    drop(store);
    let store = Store::open_data_dir(dir.path())?;
    let mut secondary = reading();
    secondary.window = "secondary".into();
    secondary.observed_at = "2026-10-20T19:01:00Z".into();
    let before = store.storage_stats()?;
    assert!(store.record_quota(&secondary)?);
    let rows = store.quota_history()?;
    assert_eq!(rows[1].balance.as_deref(), Some("12.34"));
    assert_eq!(rows[1].observed_at, secondary.observed_at);
    assert_eq!(
        store.storage_stats()?.catalog_transaction_count - before.catalog_transaction_count,
        1
    );
    secondary.account = "other".into();
    assert!(store.record_quota(&secondary)?);
    assert!(store.quota_history()?[2].balance.is_none());
    Ok(())
}

#[test]
fn v1_catalog_migrates_in_place_and_keeps_legacy_rows_null() -> Result<()> {
    let dir = tempfile::tempdir()?;
    drop(Store::open_data_dir(dir.path())?);
    // Recreate the v1 catalog exactly: no balance column, user_version 1.
    let catalog = dir.path().join("catalog.db");
    let legacy = Connection::open(&catalog)?;
    legacy.execute_batch(
        "DROP TABLE quota_history;
         CREATE TABLE quota_history(id INTEGER PRIMARY KEY AUTOINCREMENT,
             provider TEXT NOT NULL, account TEXT NOT NULL, window TEXT NOT NULL,
             data TEXT NOT NULL);
         CREATE INDEX quota_history_key ON quota_history(account,window,id);
         PRAGMA user_version=1;",
    )?;
    legacy.execute(
        "INSERT INTO quota_history(provider,account,window,data) VALUES('p','work','weekly',?1)",
        [serde_json::to_string(&reading())?],
    )?;
    drop(legacy);

    // The read-only CLI path still reads a v1 catalog without the column.
    assert_eq!(
        Store::read_quota_data_dir(dir.path(), false)?,
        vec![reading()]
    );

    let store = Store::open_data_dir(dir.path())?;
    let version: i64 =
        Connection::open(&catalog)?.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    assert_eq!(version, 2);
    assert_eq!(store.quota_history()?, vec![reading()]);
    assert!(store.latest_quota_balances()?.is_empty());

    let mut changed = reading();
    changed.used_percent = 90.0;
    changed.observed_at = "2026-10-20T19:05:00Z".into();
    changed.balance = Some("42.00".into());
    let before = store.storage_stats()?;
    assert!(store.record_quota(&changed)?);
    assert_eq!(
        store.storage_stats()?.catalog_transaction_count - before.catalog_transaction_count,
        1
    );
    let history = store.quota_history()?;
    assert_eq!(history[1].balance.as_deref(), Some("42.00"));
    assert_eq!(history[1].observed_at, changed.observed_at);

    // A balance-only change is held by the host; the store never writes it.
    let mut balance_only = changed.clone();
    balance_only.observed_at = "2026-10-20T19:06:00Z".into();
    balance_only.balance = Some("41.00".into());
    let before = store.storage_stats()?;
    assert!(!store.record_quota(&balance_only)?);
    assert_eq!(
        store.storage_stats()?.catalog_transaction_count,
        before.catalog_transaction_count
    );
    drop(store);

    // Reopening resumes from the stored balance, not from the rejected one.
    let reopened = Store::open_data_dir(dir.path())?;
    let stored = reopened.latest_quota_balances()?;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].account, "work");
    assert_eq!(stored[0].balance.as_deref(), Some("42.00"));
    assert_eq!(stored[0].observed_at, changed.observed_at);
    assert_eq!(reopened.quota_history()?, history);
    Ok(())
}

#[test]
fn invalid_quota_observations_are_rejected_without_writes() -> Result<()> {
    let store = Store::in_memory()?;
    for percent in [f64::NAN, f64::INFINITY, -0.01, 100.01] {
        let mut invalid = reading();
        invalid.used_percent = percent;
        assert!(store.record_quota(&invalid).is_err());
    }
    let mut invalid = reading();
    invalid.observed_at = "2026-10-20T21:00:00+02:00".into();
    assert!(store.record_quota(&invalid).is_err());
    assert!(store.quota_history()?.is_empty());
    assert_eq!(store.storage_stats()?.transaction_count, 0);
    assert!(store.record_quota(&reading())?);
    assert!(!store.record_quota(&reading())?);
    Ok(())
}

struct WaitingProvider {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    fail: bool,
}
#[async_trait]
impl Provider for WaitingProvider {
    async fn generate(
        &self,
        _request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        assert!(set_quota_wait(
            "work",
            "weekly",
            RESET,
            "all eligible accounts exhausted"
        )?);
        self.entered.notify_one();
        self.release.notified().await;
        if self.fail {
            anyhow::bail!("synthetic failure after quota wait");
        }
        assert!(clear_current_quota_wait()?);
        Ok(ModelResponse {
            text: "done".into(),
            ..Default::default()
        })
    }
}
fn waiting_harness(store: Store, fail: bool) -> (Harness, Arc<Notify>, Arc<Notify>) {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let engine = Harness::new(
        store,
        Arc::new(WaitingProvider {
            entered: entered.clone(),
            release: release.clone(),
            fail,
        }),
        ToolRegistry::new(),
        HarnessConfig {
            model: "p/m".into(),
            ..Default::default()
        },
    );
    (engine, entered, release)
}

#[tokio::test]
async fn quota_wait_keeps_one_live_run_and_clears_on_success_failure_and_cancel() -> Result<()> {
    for outcome in ["completed", "failed", "cancelled", "shutdown"] {
        let dir = tempfile::tempdir()?;
        let store = Store::open_data_dir(dir.path())?;
        let session = store.create_session(outcome)?;
        let (engine, entered, release) = waiting_harness(store.clone(), outcome == "failed");
        let mut events = engine.subscribe();
        let admitted = engine.start(&session.id, "accepted input", RunOptions::default())?;
        tokio::time::timeout(std::time::Duration::from_secs(3), entered.notified()).await?;
        let parked = store.run(&admitted.id)?;
        assert_eq!(parked.status, "waiting_for_quota");
        assert!(parked.ended_at.is_none());
        assert_eq!(store.pending_quota_waits()?.len(), 1);
        assert!(!store.archive_chat(&session.id)?);
        assert!(
            engine
                .start(&session.id, "second", RunOptions::default())
                .is_err()
        );
        if outcome == "cancelled" {
            let wait = store.quota_wait(&session.id)?.unwrap();
            assert!(engine.cancel(&session.id));
            assert!(
                store.quota_wait(&session.id)?.is_none(),
                "successful cancel clears restart hint synchronously"
            );
            assert!(
                store.put_quota_wait(&wait).is_err(),
                "cancelled provider cannot recreate the hint"
            );
        } else if outcome == "shutdown" {
            engine.shutdown(std::time::Duration::from_secs(3)).await?;
        } else {
            release.notify_one();
        }
        let finished = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            engine.wait_for_idle(&session.id),
        )
        .await??;
        assert_eq!(finished.id, admitted.id);
        assert_eq!(
            finished.status,
            if outcome == "shutdown" {
                "cancelled"
            } else {
                outcome
            }
        );
        assert!(store.pending_quota_waits()?.is_empty());
        assert_eq!(
            store
                .messages(&session.id)?
                .iter()
                .filter(|m| m.content == "accepted input")
                .count(),
            1
        );
        let observed: Vec<_> = std::iter::from_fn(|| events.try_recv().ok()).collect();
        assert!(
            observed
                .iter()
                .any(|e| matches!(e.kind, EventKind::RunWaitingForQuota { .. }))
        );
        assert_eq!(
            observed
                .iter()
                .filter(|e| matches!(e.kind, EventKind::RunStarted { .. }))
                .count(),
            1
        );
        assert!(
            engine
                .events_after(0, Some(&session.id), 100)?
                .iter()
                .any(|e| matches!(e.kind, EventKind::RunWaitingForQuota { .. }))
        );
    }
    assert!(!set_quota_wait("work", "weekly", RESET, "standalone")?);
    Ok(())
}

#[derive(Default)]
struct CancelObservations(Mutex<Vec<ObservationRecord>>);
impl ObservationSink for CancelObservations {
    fn record(&self, record: ObservationRecord) {
        self.0.lock().unwrap().push(record);
    }
}

#[tokio::test]
async fn quota_cancel_does_not_acknowledge_failed_durable_marker_removal() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("cancel storage failure")?;
    let entered = Arc::new(Notify::new());
    let observations = Arc::new(CancelObservations::default());
    let engine = Harness::new(
        store.clone(),
        Arc::new(WaitingProvider {
            entered: entered.clone(),
            release: Arc::new(Notify::new()),
            fail: false,
        }),
        ToolRegistry::new(),
        HarnessConfig {
            model: "p/m".into(),
            observation: Some(Observation {
                sink: observations.clone(),
                mode: ObservationMode::Contents,
                max_payload_bytes: 2048,
            }),
            ..Default::default()
        },
    );
    let run = engine.start(&session.id, "accepted input", RunOptions::default())?;
    tokio::time::timeout(std::time::Duration::from_secs(3), entered.notified()).await?;
    let marker = store.quota_wait(&session.id)?.unwrap();
    let connection = Connection::open(store.chat_path(&session.id)?.unwrap())?;
    connection.execute_batch(
        "CREATE TRIGGER reject_quota_cancel BEFORE DELETE ON metadata
         WHEN substr(OLD.key,1,11)='quota_wait:'
         BEGIN SELECT RAISE(ABORT,'synthetic quota cancellation storage failure'); END;",
    )?;
    assert!(
        !engine.cancel(&session.id),
        "a volatile cancellation token cannot acknowledge durable cancellation"
    );
    assert_eq!(store.quota_wait(&session.id)?, Some(marker));
    {
        let records = observations.0.lock().unwrap();
        let failure = records
            .iter()
            .find(|record| record.kind == "quota_cancel_failed")
            .expect("durable cancellation failure is observable");
        assert_eq!(failure.scope.session_id, session.id);
        assert_eq!(failure.scope.run_id, run.id);
        assert_eq!(failure.metrics["error_category"], "storage_error");
        assert!(
            failure
                .payload
                .as_deref()
                .unwrap()
                .contains("synthetic quota cancellation storage failure")
        );
    }
    // Do not yield: the cancelled drive task has not finalized yet. Retrying
    // after storage recovers must durably clear the hint before reporting true.
    connection.execute_batch("DROP TRIGGER reject_quota_cancel;")?;
    assert!(engine.cancel(&session.id));
    assert!(store.quota_wait(&session.id)?.is_none());
    let finished = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        engine.wait_for_idle(&session.id),
    )
    .await??;
    assert_eq!(finished.status, "cancelled");
    assert!(store.pending_quota_waits()?.is_empty());
    Ok(())
}

#[test]
fn quota_cancel_process_helper() -> Result<()> {
    let Some(directory) = std::env::var_os("BUBBLEPAW_QUOTA_CANCEL_CHILD") else {
        return Ok(());
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let store = Store::open_data_dir(directory)?;
            let session = store.create_session("cancel then crash")?;
            let (engine, entered, _) = waiting_harness(store.clone(), false);
            engine.start(&session.id, "accepted input", RunOptions::default())?;
            entered.notified().await;
            assert!(engine.cancel(&session.id));
            assert!(store.quota_wait(&session.id)?.is_none());
            // No yield and no destructors: the drive task has not finalized its
            // cancelled run. Only the synchronous cancellation intent is durable.
            std::process::exit(0);
            #[allow(unreachable_code)]
            Ok(())
        })
}

#[test]
fn successful_cancel_survives_process_exit_before_run_finalization() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let child = std::process::Command::new(std::env::current_exe()?)
        .args(["--exact", "quota_cancel_process_helper", "--nocapture"])
        .env("BUBBLEPAW_QUOTA_CANCEL_CHILD", dir.path())
        .output()?;
    assert!(
        child.status.success(),
        "child failed: {}",
        String::from_utf8_lossy(&child.stderr)
    );
    let chat = std::fs::read_dir(dir.path().join("chats"))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|extension| extension == "db"))
        .unwrap();
    let connection = Connection::open(chat)?;
    assert_eq!(
        connection.query_row("SELECT status FROM runs", [], |row| row.get::<_, String>(0))?,
        "waiting_for_quota"
    );
    assert_eq!(
        connection.query_row(
            "SELECT COUNT(*) FROM metadata WHERE substr(key,1,11)='quota_wait:'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    drop(connection);
    let store = Store::open_data_dir(dir.path())?;
    assert!(store.pending_quota_waits()?.is_empty());
    let session = store.list_sessions()?.remove(0);
    assert_eq!(store.runs(&session.id)?[0].status, "interrupted");
    assert_eq!(
        store
            .messages(&session.id)?
            .iter()
            .filter(|message| message.content == "accepted input")
            .count(),
        1
    );
    Ok(())
}

fn interrupted_wait(store: &Store, session_id: &str) -> Result<QuotaWait> {
    interrupted_wait_at(store, session_id, RESET)
}

fn interrupted_wait_at(store: &Store, session_id: &str, resume_at: &str) -> Result<QuotaWait> {
    let accepted = serde_json::from_value(serde_json::json!({
        "id": uuid::Uuid::new_v4().to_string(), "session_id": session_id,
        "role": "user", "content": "original accepted input", "created_at": DATE,
    }))?;
    store.append_batch(&[accepted])?;
    let run = Run {
        id: uuid::Uuid::new_v4().to_string(),
        session_id: session_id.into(),
        model: "p/m".into(),
        status: "running".into(),
        text: None,
        error: None,
        started_at: DATE.into(),
        ended_at: None,
        usage: None,
    };
    let connection = Connection::open(store.chat_path(session_id)?.unwrap())?;
    connection.execute(
        "INSERT INTO runs(id,session_id,status,data) VALUES(?1,?2,'running',?3)",
        params![run.id, session_id, serde_json::to_string(&run)?],
    )?;
    let wait = QuotaWait {
        session_id: session_id.into(),
        run_id: run.id,
        account: "work".into(),
        window: "weekly".into(),
        resume_at: resume_at.into(),
        reason: "all exhausted".into(),
    };
    store.put_quota_wait(&wait)?;
    Ok(wait)
}

#[test]
fn unique_active_run_index_includes_parked_runs_after_creation_and_schema_eight_migration()
-> Result<()> {
    for migrate in [false, true] {
        let dir = tempfile::tempdir()?;
        let store = Store::open_data_dir(dir.path())?;
        let session = store.create_session("one active run")?;
        let chat = store.chat_path(&session.id)?.unwrap();
        drop(store);
        if migrate {
            Connection::open(&chat)?.execute_batch(
                "ALTER TABLE request_usage DROP COLUMN account;
                 DROP INDEX one_running;
                 CREATE UNIQUE INDEX one_running ON runs(session_id) WHERE status='running';
                 PRAGMA user_version=8; PRAGMA wal_checkpoint(TRUNCATE);",
            )?;
        }
        let store = Store::open_data_dir(dir.path())?;
        let wait = interrupted_wait(&store, &session.id)?;
        let connection = Connection::open(&chat)?;
        assert_eq!(
            connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?,
            12
        );
        let second = Run {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: session.id.clone(),
            model: "p/m".into(),
            status: "running".into(),
            text: None,
            error: None,
            started_at: DATE.into(),
            ended_at: None,
            usage: None,
        };
        let error = connection
            .execute(
                "INSERT INTO runs(id,session_id,status,data) VALUES(?1,?2,'running',?3)",
                params![second.id, session.id, serde_json::to_string(&second)?],
            )
            .expect_err("a parked run retains exclusive SQL admission");
        assert_eq!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ConstraintViolation)
        );
        assert_eq!(store.run(&wait.run_id)?.status, "waiting_for_quota");
        assert_eq!(store.runs(&session.id)?.len(), 1);
        assert_eq!(store.quota_wait(&session.id)?.as_ref(), Some(&wait));
    }
    Ok(())
}

#[test]
fn zero_retention_preserves_canonical_interrupted_quota_wait_and_its_events() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let options = StoreOptions {
        finished_event_retention_hours: 0,
        telemetry_retention_days: 0,
        ..Default::default()
    };
    let store = Store::open_with_options(dir.path(), options)?;
    let session = store.create_session("quota wait survives retention")?;
    let wait = interrupted_wait(&store, &session.id)?;
    // Flush the disposable event journal before simulating process restart.
    store.housekeeping()?;
    drop(store);
    let store = Store::open_with_options(dir.path(), options)?;
    assert_eq!(store.run(&wait.run_id)?.status, "interrupted");
    assert_eq!(store.quota_wait(&session.id)?.as_ref(), Some(&wait));

    // An unrelated historical interrupted run in the disposable projection
    // remains eligible for normal retention. It does not own this marker.
    let mut unrelated = store.run(&wait.run_id)?;
    unrelated.id = uuid::Uuid::new_v4().to_string();
    unrelated.started_at = "2020-01-01T00:00:00Z".into();
    unrelated.ended_at = Some("2020-01-01T00:00:01Z".into());
    let serving = Connection::open(dir.path().join("serving.db"))?;
    serving.execute(
        "INSERT INTO runs(id,session_id,status,data) VALUES(?1,?2,'interrupted',?3)",
        params![unrelated.id, session.id, serde_json::to_string(&unrelated)?],
    )?;
    drop(serving);
    store.update_host_state(&session.id, &unrelated.id, "retention_fixture", |_| {
        Ok(serde_json::json!(true))
    })?;
    store.housekeeping()?;

    assert_eq!(store.run(&wait.run_id)?.status, "interrupted");
    assert_eq!(store.pending_quota_waits()?, vec![wait.clone()]);
    let chat = Connection::open(store.chat_path(&session.id)?.unwrap())?;
    assert_eq!(
        chat.query_row(
            "SELECT COUNT(*) FROM runs WHERE id=?1",
            [&wait.run_id],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );
    let serving = Connection::open(dir.path().join("serving.db"))?;
    assert_eq!(
        serving.query_row(
            "SELECT COUNT(*) FROM runs WHERE id=?1",
            [&unrelated.id],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    assert_eq!(
        serving.query_row(
            "SELECT COUNT(*) FROM events WHERE run_id=?1",
            [&unrelated.id],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    assert_eq!(
        serving.query_row(
            "SELECT COUNT(*) FROM events WHERE run_id=?1",
            [&wait.run_id],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );
    Ok(())
}

struct Done;
#[async_trait]
impl Provider for Done {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        assert_eq!(
            request
                .messages
                .iter()
                .filter(|message| message.content == "original accepted input")
                .count(),
            1
        );
        Ok(ModelResponse {
            text: "recovered".into(),
            ..Default::default()
        })
    }
}
fn recovery_harness(store: Store) -> Harness {
    Harness::new(
        store,
        Arc::new(Done),
        ToolRegistry::new(),
        HarnessConfig {
            model: "p/m".into(),
            ..Default::default()
        },
    )
}

#[tokio::test]
async fn restart_retains_hint_until_explicit_new_run_or_cancel() -> Result<()> {
    for cancel in [false, true] {
        let dir = tempfile::tempdir()?;
        let store = Store::open_data_dir(dir.path())?;
        let session = store.create_session("restart")?;
        let wait = interrupted_wait(&store, &session.id)?;
        drop(store);
        let store = Store::open_data_dir(dir.path())?;
        assert_eq!(store.pending_quota_waits()?, vec![wait.clone()]);
        assert_eq!(store.run(&wait.run_id)?.status, "interrupted");
        assert_eq!(
            store
                .messages(&session.id)?
                .iter()
                .filter(|message| message.content == "original accepted input")
                .count(),
            1
        );
        assert!(!store.archive_chat(&session.id)?);
        store.serving_wipe()?;
        assert_eq!(store.pending_quota_waits()?, vec![wait.clone()]);
        let engine = recovery_harness(store.clone());
        if cancel {
            assert!(engine.cancel(&session.id));
            assert_eq!(store.run(&wait.run_id)?.status, "cancelled");
        } else {
            let resumed = engine.resume(&session.id, RunOptions::default()).await?;
            assert_eq!(resumed.status, "completed");
            assert_ne!(resumed.id, wait.run_id);
            assert!(!store.clear_quota_wait(&session.id, &wait.run_id)?);
        }
        assert!(store.pending_quota_waits()?.is_empty());
    }
    Ok(())
}

#[test]
fn cleared_wait_is_not_recovered_and_scheduler_failure_is_terminal() -> Result<()> {
    for clear in [false, true] {
        let dir = tempfile::tempdir()?;
        let store = Store::open_data_dir(dir.path())?;
        let session = store.create_session("recovery failure")?;
        let wait = interrupted_wait(&store, &session.id)?;
        if clear {
            assert!(store.clear_quota_wait(&session.id, &wait.run_id)?);
        }
        drop(store);
        let store = Store::open_data_dir(dir.path())?;
        if clear {
            assert!(store.pending_quota_waits()?.is_empty());
            assert_eq!(store.run(&wait.run_id)?.status, "interrupted");
        } else {
            assert_eq!(store.pending_quota_waits()?.len(), 1);
            assert!(store.fail_quota_wait(
                &session.id,
                &wait.run_id,
                "configured profile no longer exists"
            )?);
            assert!(store.pending_quota_waits()?.is_empty());
            let run = store.run(&wait.run_id)?;
            assert_eq!(run.status, "failed");
            assert_eq!(
                run.error.as_deref(),
                Some("configured profile no longer exists")
            );
            assert!(!store.fail_quota_wait(&session.id, &wait.run_id, "retry")?);
        }
    }
    Ok(())
}

#[tokio::test]
async fn automatic_resume_atomically_requires_the_same_due_interrupted_wait() -> Result<()> {
    for change in [
        "cancel",
        "explicit_resume",
        "unchanged",
        "future",
        "changed_marker",
    ] {
        let dir = tempfile::tempdir()?;
        let store = Store::open_data_dir(dir.path())?;
        let session = store.create_session(change)?;
        let reset = if change == "future" {
            (chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339()
        } else {
            (chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339()
        };
        let wait = interrupted_wait_at(&store, &session.id, &reset)?;
        // A live wait is never an automatic restart candidate.
        let live_engine = recovery_harness(store.clone());
        assert!(
            live_engine
                .start_quota_resume(&wait, RunOptions::default())
                .is_err()
        );
        drop(live_engine);
        drop(store);
        let store = Store::open_data_dir(dir.path())?;
        let engine = recovery_harness(store.clone());
        // The scheduler has observed this exact marker; another client can
        // cancel or explicitly resume before it reaches atomic admission.
        assert_eq!(store.quota_wait(&session.id)?.as_ref(), Some(&wait));
        if change == "cancel" {
            assert!(engine.cancel(&session.id));
        }
        if change == "explicit_resume" {
            engine.resume(&session.id, RunOptions::default()).await?;
        }
        if change == "changed_marker" {
            let mut newer = wait.clone();
            newer.reason = "newer wait state".into();
            Connection::open(store.chat_path(&session.id)?.unwrap())?.execute(
                "UPDATE metadata SET value=?2 WHERE key=?1",
                params![
                    format!("quota_wait:{}", session.id),
                    serde_json::to_string(&newer)?
                ],
            )?;
        }
        let before_runs = store.runs(&session.id)?.len();
        let before = store.storage_stats()?;
        let result = engine.start_quota_resume(&wait, RunOptions::default());
        if change == "unchanged" {
            let admitted = result?;
            assert_ne!(admitted.id, wait.run_id);
            assert_eq!(engine.wait_for_idle(&session.id).await?.status, "completed");
            assert!(store.quota_wait(&session.id)?.is_none());
        } else {
            let error = result.expect_err("stale or premature automatic admission rejected");
            if change != "future" {
                assert!(error.downcast_ref::<QuotaWaitChanged>().is_some());
            }
            assert_eq!(store.runs(&session.id)?.len(), before_runs);
            assert_eq!(
                store.storage_stats()?.chat_transaction_count,
                before.chat_transaction_count
            );
        }
    }
    Ok(())
}

#[test]
fn unresolved_effect_blocks_park_and_discards_restart_hint() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("unknown effect")?;
    let wait = interrupted_wait(&store, &session.id)?;
    let connection = Connection::open(store.chat_path(&session.id)?.unwrap())?;
    connection.execute("INSERT INTO effects(id,session_id,run_id,call_id,state) VALUES('effect',?1,?2,'call','dispatched')",
        params![session.id, wait.run_id])?;
    assert!(store.put_quota_wait(&wait).is_err());
    drop(connection);
    drop(store);
    let store = Store::open_data_dir(dir.path())?;
    assert!(store.pending_quota_waits()?.is_empty());
    let connection = Connection::open(store.chat_path(&session.id)?.unwrap())?;
    assert_eq!(
        connection.query_row("SELECT state FROM effects WHERE id='effect'", [], |row| row
            .get::<_, String>(0))?,
        "unknown"
    );
    Ok(())
}
