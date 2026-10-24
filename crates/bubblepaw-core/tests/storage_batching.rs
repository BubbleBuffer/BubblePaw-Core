mod support;

use anyhow::Result;
use bubblepaw_core::{Role, Store, transcript::ImportDerivedRecord};
use rusqlite::Connection;
use serde_json::json;
use support::stored_text;

#[test]
fn batches_projection_writes_and_reads_pending_search_immediately() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("batch")?;
    let before = store.storage_stats()?;
    for i in 0..12 {
        store.append_batch(&[stored_text(
            &session.id,
            &format!("m{i}"),
            "marigold camera",
        )])?;
    }
    let pending = store.storage_stats()?;
    assert_eq!(
        pending.chat_transaction_count - before.chat_transaction_count,
        12
    );
    assert_eq!(
        pending.serving_transaction_count,
        before.serving_transaction_count
    );
    assert_eq!(
        pending.catalog_transaction_count,
        before.catalog_transaction_count
    );
    assert_eq!(store.search_messages("marigold", None, 100)?.len(), 12);
    store.flush_serving()?;
    let after = store.storage_stats()?;
    assert_eq!(
        after.serving_transaction_count - before.serving_transaction_count,
        1
    );
    assert_eq!(
        after.catalog_transaction_count - before.catalog_transaction_count,
        0
    );
    let conn = Connection::open(dir.path().join("serving.db"))?;
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM search_rows", [], |row| row
            .get::<_, i64>(0))?,
        12
    );
    Ok(())
}

#[test]
fn graceful_close_flushes_but_wipe_can_drop_pending_projection() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let session;
    {
        let store = Store::open_data_dir(dir.path())?;
        session = store.create_session("graceful")?.id;
        store.append_batch(&[stored_text(&session, "one", "saffron archive")])?;
    }
    let store = Store::open_data_dir(dir.path())?;
    assert_eq!(store.search_messages("saffron", None, 10)?.len(), 1);
    store.append_batch(&[stored_text(&session, "two", "cerulean archive")])?;
    store.serving_wipe()?;
    assert!(store.search_messages("cerulean", None, 10)?.is_empty());
    assert_eq!(store.messages(&session)?.len(), 2);
    assert!(store.reindex_search(None, 20)?.complete);
    assert_eq!(store.search_messages("cerulean", None, 10)?.len(), 1);
    Ok(())
}

#[test]
fn imported_serving_summary_does_not_decode_covered_rows() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("summary seed")?;
    store.append_import_batch(
        &[
            stored_text(&session.id, "covered", "large old history"),
            stored_text(&session.id, "tail", "new tail"),
        ],
        &[ImportDerivedRecord {
            id: "compaction".into(),
            session_id: session.id.clone(),
            source_line: 3,
            source_kind: "codex_compaction".into(),
            through_message_id: Some("covered".into()),
            content: json!({"message":"retained imported summary"}),
            created_at: "2026-10-12T00:00:00Z".into(),
            origin: "import".into(),
        }],
    )?;
    Connection::open(store.chat_path(&session.id)?.unwrap())?.execute(
        "UPDATE messages SET data='invalid covered body' WHERE message_id='covered'",
        [],
    )?;
    let serving = store.serving_messages(&session.id)?;
    assert_eq!(serving.len(), 2);
    assert_eq!(serving[0].content, "retained imported summary");
    assert_eq!(serving[1].id, "tail");
    Ok(())
}

struct Reuse;
#[async_trait::async_trait]
impl bubblepaw_core::Provider for Reuse {
    async fn generate(
        &self,
        request: bubblepaw_core::ModelRequest,
        _: bubblepaw_core::TextCallback,
        _: bubblepaw_core::CancellationToken,
    ) -> Result<bubblepaw_core::ModelResponse> {
        assert_eq!(request.messages[0].content, "summary of previous tool call");
        assert!(
            !request
                .messages
                .iter()
                .any(|message| message.id == "old-call")
        );
        Ok(bubblepaw_core::ModelResponse {
            tool_calls: vec![bubblepaw_core::ToolCall {
                id: "durable-old-id".into(),
                name: "no-replay".into(),
                arguments: json!({}),
            }],
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn lazy_imported_history_still_refuses_reused_tool_call_ids() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("covered tool call")?;
    let mut old = stored_text(&session.id, "old-call", "");
    old.role = Role::Assistant;
    old.tool_calls = vec![bubblepaw_core::ToolCall {
        id: "durable-old-id".into(),
        name: "no-replay".into(),
        arguments: json!({}),
    }];
    store.append_import_batch(
        &[old],
        &[ImportDerivedRecord {
            id: "summary".into(),
            session_id: session.id.clone(),
            source_line: 2,
            source_kind: "codex_compaction".into(),
            through_message_id: Some("old-call".into()),
            content: json!({"message":"summary of previous tool call"}),
            created_at: "2026-10-12T00:00:00Z".into(),
            origin: "import".into(),
        }],
    )?;
    let harness = bubblepaw_core::Harness::new(
        store,
        std::sync::Arc::new(Reuse),
        bubblepaw_core::ToolRegistry::new(),
        bubblepaw_core::HarnessConfig::default(),
    );
    let run = harness
        .prompt(
            &session.id,
            "continue",
            bubblepaw_core::RunOptions::default(),
        )
        .await?;
    assert_eq!(run.status, "failed");
    assert!(run.error.unwrap_or_default().contains("reused"));
    Ok(())
}

struct NativeModel;

struct PlainModel;
#[async_trait::async_trait]
impl bubblepaw_core::Provider for PlainModel {
    async fn generate(
        &self,
        _: bubblepaw_core::ModelRequest,
        _: bubblepaw_core::TextCallback,
        _: bubblepaw_core::CancellationToken,
    ) -> Result<bubblepaw_core::ModelResponse> {
        Ok(bubblepaw_core::ModelResponse {
            text: "Synthetic retained reply. ".repeat(1000),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn routine_idle_bounds_wal_and_explicit_checkpoint_truncates_it() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("bounded WAL")?;
    let harness = bubblepaw_core::Harness::new(
        store.clone(),
        std::sync::Arc::new(PlainModel),
        bubblepaw_core::ToolRegistry::new(),
        bubblepaw_core::HarnessConfig::default(),
    );
    let paths = [
        store.chat_path(&session.id)?.unwrap(),
        dir.path().join("serving.db"),
        dir.path().join("catalog.db"),
    ];
    let wal_size = |path: &std::path::Path| {
        std::fs::metadata(format!("{}-wal", path.display())).map_or(0, |metadata| metadata.len())
    };
    for index in 0..36 {
        assert_eq!(
            harness
                .prompt(
                    &session.id,
                    format!("Synthetic input {index}"),
                    bubblepaw_core::RunOptions::default()
                )
                .await?
                .status,
            "completed"
        );
        for path in &paths {
            assert!(
                wal_size(path) < 1_048_576,
                "Routine idle must bound {}",
                path.display()
            );
        }
        if index == 0 {
            assert!(
                paths.iter().any(|path| wal_size(path) > 0),
                "Small idle WALs should be reclaimed together later"
            );
        }
    }
    assert_eq!(store.messages(&session.id)?.len(), 72);
    store.idle_checkpoint()?;
    assert!(paths.iter().all(|path| wal_size(path) == 0));
    assert_eq!(
        Connection::open(&paths[0])?
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row
                .get::<_, i64>(0))?,
        72
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routine_idle_archives_an_eligible_other_chat_before_notifying() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let old = store.create_session("old root")?;
    let old_path = store.chat_path(&old.id)?.unwrap();
    let current = store.create_session("current root")?;
    store.flush_serving()?;
    Connection::open(dir.path().join("catalog.db"))?.execute(
        "UPDATE sessions SET last_activity='2000-01-01T00:00:00Z' WHERE id=?1",
        [&old.id],
    )?;
    let harness = bubblepaw_core::Harness::new(
        store.clone(),
        std::sync::Arc::new(PlainModel),
        bubblepaw_core::ToolRegistry::new(),
        bubblepaw_core::HarnessConfig::default(),
    );
    let run = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        harness.prompt(
            &current.id,
            "Synthetic current input",
            bubblepaw_core::RunOptions::default(),
        ),
    )
    .await??;
    assert_eq!(run.status, "completed");
    assert!(!old_path.exists());
    assert!(std::path::PathBuf::from(format!("{}.zst", old_path.display())).is_file());
    assert_eq!(store.messages(&current.id)?.len(), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn admission_waits_for_maintenance_then_answers_late_input() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("maintenance admission")?;
    let harness = bubblepaw_core::Harness::new(
        store.clone(),
        std::sync::Arc::new(PlainModel),
        bubblepaw_core::ToolRegistry::new(),
        bubblepaw_core::HarnessConfig::default(),
    );
    let catalog = Connection::open(dir.path().join("catalog.db"))?;
    catalog.execute_batch("BEGIN IMMEDIATE")?;
    let mut events = harness.subscribe();
    let first = harness.start(
        &session.id,
        "first prompt",
        bubblepaw_core::RunOptions::default(),
    )?;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if matches!(
                events.recv().await?.kind,
                bubblepaw_core::EventKind::SessionSettled {}
            ) {
                break;
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(25),
            harness.wait_for_idle(&session.id)
        )
        .await
        .is_err(),
        "Idle cannot be published before maintenance finishes"
    );
    let late_harness = harness.clone();
    let late_session = session.id.clone();
    let late = tokio::task::spawn_blocking(move || -> Result<_> {
        late_harness.followup(&late_session, "late followup")?;
        late_harness.start(
            &late_session,
            "second prompt",
            bubblepaw_core::RunOptions::default(),
        )
    });
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert!(
        !late.is_finished(),
        "Admission must wait without owning the archival admission lock"
    );
    catalog.execute_batch("ROLLBACK")?;
    let second = tokio::time::timeout(std::time::Duration::from_secs(10), late).await???;
    assert_ne!(first.id, second.id);
    assert_eq!(
        harness.wait_for_idle(&session.id).await?.status,
        "completed"
    );
    let messages = store.messages(&session.id)?;
    for text in ["first prompt", "late followup", "second prompt"] {
        assert!(
            messages
                .iter()
                .any(|message| message.role == Role::User && message.content == text)
        );
    }
    assert!(
        !store
            .queued_inputs(&session.id)?
            .iter()
            .any(|input| input.state == "pending")
    );
    Ok(())
}

#[async_trait::async_trait]
impl bubblepaw_core::Provider for NativeModel {
    async fn generate(
        &self,
        request: bubblepaw_core::ModelRequest,
        _: bubblepaw_core::TextCallback,
        _: bubblepaw_core::CancellationToken,
    ) -> Result<bubblepaw_core::ModelResponse> {
        Ok(
            if request
                .messages
                .iter()
                .any(|message| message.tool_call_id.is_some())
            {
                bubblepaw_core::ModelResponse {
                    text: "finished".into(),
                    ..Default::default()
                }
            } else {
                bubblepaw_core::ModelResponse {
                    tool_calls: vec![bubblepaw_core::ToolCall {
                        id: "native-effect".into(),
                        name: "verify_marker".into(),
                        arguments: json!({}),
                    }],
                    ..Default::default()
                }
            },
        )
    }
}
struct VerifyMarker(std::path::PathBuf);
#[async_trait::async_trait]
impl bubblepaw_core::Tool for VerifyMarker {
    fn definition(&self) -> bubblepaw_core::ToolDefinition {
        bubblepaw_core::ToolDefinition {
            name: "verify_marker".into(),
            description: "Synthetic boundary check".into(),
            input_schema: json!({"type":"object"}),
            read_only: false,
            approval: Default::default(),
        }
    }
    async fn invoke(
        &self,
        context: bubblepaw_core::ToolContext,
        _: serde_json::Value,
    ) -> Result<bubblepaw_core::ToolResult> {
        let conn =
            Connection::open_with_flags(&self.0, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let state: String = conn.query_row(
            "SELECT state FROM effects WHERE id=?1",
            [&context.invocation_id],
            |row| row.get(0),
        )?;
        assert_eq!(state, "dispatched");
        let calls:i64=conn.query_row("SELECT COUNT(*) FROM message_tool_calls WHERE session_id=?1 AND call_id='native-effect'",[&context.session_id],|row|row.get(0))?;
        assert_eq!(calls, 1);
        let usage: i64 = conn.query_row(
            "SELECT COUNT(*) FROM request_usage WHERE session_id=?1 AND run_id=?2",
            [&context.session_id, &context.run_id],
            |row| row.get(0),
        )?;
        assert_eq!(
            usage, 1,
            "the model request is durable before external dispatch"
        );
        Ok(bubblepaw_core::ToolResult::text("marker verified"))
    }
}

#[tokio::test]
async fn single_allow_turn_commits_five_times_and_dispatches_after_record_commit() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("native boundary")?;
    let mut tools = bubblepaw_core::ToolRegistry::new();
    tools.register(std::sync::Arc::new(VerifyMarker(
        store.chat_path(&session.id)?.unwrap(),
    )))?;
    let harness = bubblepaw_core::Harness::new(
        store.clone(),
        std::sync::Arc::new(NativeModel),
        tools,
        bubblepaw_core::HarnessConfig::default(),
    );
    let before = store.storage_stats()?;
    let run = harness
        .prompt(
            &session.id,
            "check the marker",
            bubblepaw_core::RunOptions::default(),
        )
        .await?;
    assert_eq!(run.status, "completed");
    let after = store.storage_stats()?;
    assert_eq!(
        after.chat_transaction_count - before.chat_transaction_count,
        4
    );
    assert_eq!(
        after.serving_transaction_count - before.serving_transaction_count,
        1
    );
    assert_eq!(
        after.catalog_transaction_count - before.catalog_transaction_count,
        0
    );
    assert_eq!(after.transaction_count - before.transaction_count, 5);
    assert_eq!(store.messages(&session.id)?.len(), 4);
    // Both model steps piggyback their assistant commits: the baseline remains
    // four canonical chat commits plus one buffered serving commit.
    let usage = store.request_usage()?;
    println!(
        "native tool request usage: rows={}, chat_tx={}, serving_tx={}, catalog_tx={}, database_bytes={}->{}, wal_bytes={}->{}, chat_bytes={}->{}, serving_bytes={}->{}",
        usage.len(),
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
    assert_eq!(usage.len(), 2);
    assert!(
        usage
            .iter()
            .all(|row| row.run_id.as_deref() == Some(run.id.as_str()))
    );
    Ok(())
}

#[tokio::test]
async fn rejected_operational_copy_marks_journal_gap_without_losing_messages() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("projection failure")?;
    let serving = Connection::open(dir.path().join("serving.db"))?;
    serving.execute_batch("CREATE TRIGGER reject_resolved_input BEFORE INSERT ON inputs BEGIN SELECT RAISE(ABORT,'synthetic metadata failure'); END;")?;
    let mut tools = bubblepaw_core::ToolRegistry::new();
    tools.register(std::sync::Arc::new(VerifyMarker(
        store.chat_path(&session.id)?.unwrap(),
    )))?;
    let harness = bubblepaw_core::Harness::new(
        store.clone(),
        std::sync::Arc::new(NativeModel),
        tools,
        bubblepaw_core::HarnessConfig::default(),
    );
    let mut events = harness.subscribe();
    let run = harness
        .prompt(
            &session.id,
            "accepted despite projection failure",
            bubblepaw_core::RunOptions::default(),
        )
        .await?;
    assert_eq!(run.status, "completed");
    assert_eq!(
        store.messages(&session.id)?[0].content,
        "accepted despite projection failure"
    );
    assert!(!harness.can_resume_from(0));
    let first = events.try_recv()?;
    assert!(!first.durable);
    assert!(harness.pruned_through() >= first.sequence);
    Ok(())
}

struct PendingModel;
#[async_trait::async_trait]
impl bubblepaw_core::Provider for PendingModel {
    async fn generate(
        &self,
        _: bubblepaw_core::ModelRequest,
        _: bubblepaw_core::TextCallback,
        _: bubblepaw_core::CancellationToken,
    ) -> Result<bubblepaw_core::ModelResponse> {
        std::future::pending().await
    }
}
struct FinalModel;
#[async_trait::async_trait]
impl bubblepaw_core::Provider for FinalModel {
    async fn generate(
        &self,
        _: bubblepaw_core::ModelRequest,
        _: bubblepaw_core::TextCallback,
        _: bubblepaw_core::CancellationToken,
    ) -> Result<bubblepaw_core::ModelResponse> {
        Ok(bubblepaw_core::ModelResponse {
            text: "synthetic reply".into(),
            ..Default::default()
        })
    }
}

#[tokio::test]
#[ignore = "subprocess crash fixture; invoked only by its parent test"]
async fn abrupt_journal_child() -> Result<()> {
    let directory = std::env::var("BUBBLEPAW_BATCH_CRASH_DIR")?;
    let store = Store::open_data_dir(&directory)?;
    let session = store.create_session("crash fixture")?;
    let harness = bubblepaw_core::Harness::new(
        store,
        std::sync::Arc::new(PendingModel),
        bubblepaw_core::ToolRegistry::new(),
        bubblepaw_core::HarnessConfig::default(),
    );
    harness.start(
        &session.id,
        "durably accepted input",
        bubblepaw_core::RunOptions::default(),
    )?;
    let pending = std::path::Path::new(&directory).join("ready.part");
    std::fs::write(
        &pending,
        serde_json::to_vec(&json!({"session":session.id,"sequence":harness.last_sequence()}))?,
    )?;
    std::fs::rename(pending, std::path::Path::new(&directory).join("ready.json"))?;
    std::future::pending::<()>().await;
    Ok(())
}

#[tokio::test]
async fn abrupt_loss_of_buffered_events_never_reuses_a_client_sequence() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut child = std::process::Command::new(std::env::current_exe()?)
        .args(["--exact", "abrupt_journal_child", "--ignored"])
        .env("BUBBLEPAW_BATCH_CRASH_DIR", dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let ready = dir.path().join("ready.json");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !ready.is_file() && std::time::Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            anyhow::bail!("Crash child exited before admission");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    if !ready.is_file() {
        child.kill()?;
        child.wait()?;
        anyhow::bail!("Crash child admission timed out");
    }
    child.kill()?;
    child.wait()?;
    let admitted: serde_json::Value = serde_json::from_slice(&std::fs::read(ready)?)?;
    let session = admitted["session"].as_str().unwrap();
    let cursor = admitted["sequence"].as_u64().unwrap();
    let store = Store::open_data_dir(dir.path())?;
    assert_eq!(
        store.messages(session)?[0].content,
        "durably accepted input"
    );
    let harness = bubblepaw_core::Harness::new(
        store,
        std::sync::Arc::new(FinalModel),
        bubblepaw_core::ToolRegistry::new(),
        bubblepaw_core::HarnessConfig::default(),
    );
    assert!(harness.last_sequence() > cursor);
    for _ in 0..12 {
        assert_eq!(
            harness
                .prompt(session, "new turn", bubblepaw_core::RunOptions::default())
                .await?
                .status,
            "completed"
        );
        assert!(!harness.can_resume_from(cursor));
    }
    assert!(
        harness
            .events_after(cursor, Some(session), 1000)?
            .iter()
            .all(|event| event.sequence > cursor)
    );
    Ok(())
}

#[tokio::test]
async fn missing_both_projection_receipts_uses_exact_browser_sequence_ids() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let session;
    {
        let store = Store::open_data_dir(dir.path())?;
        session = store.create_session("lost projections")?.id;
        store.append_batch(&[stored_text(
            &session,
            "history",
            "surviving canonical history",
        )])?;
    }
    std::fs::remove_file(dir.path().join("catalog.db"))?;
    std::fs::remove_file(dir.path().join("serving.db"))?;
    let store = Store::open_data_dir(dir.path())?;
    let harness = bubblepaw_core::Harness::new(
        store,
        std::sync::Arc::new(FinalModel),
        bubblepaw_core::ToolRegistry::new(),
        bubblepaw_core::HarnessConfig::default(),
    );
    let floor = harness.last_sequence();
    assert!(floor > 1 << 32);
    assert!(floor < 1 << 53);
    assert_eq!(
        harness
            .prompt(&session, "continue", bubblepaw_core::RunOptions::default())
            .await?
            .status,
        "completed"
    );
    for event in harness.events_after(floor, Some(&session), 10)? {
        assert_eq!((event.sequence as f64) as u64, event.sequence);
    }
    Ok(())
}
