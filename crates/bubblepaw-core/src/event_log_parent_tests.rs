use super::*;
use crate::{
    CancellationToken, HarnessConfig, ModelRequest, ModelResponse, Provider, RunOptions,
    TextCallback, ToolRegistry,
};
use async_trait::async_trait;
use std::{sync::Arc, time::Duration};

struct Model;
#[async_trait]
impl Provider for Model {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        if request
            .messages
            .iter()
            .any(|message| message.content == "fail")
        {
            anyhow::bail!("scripted failure");
        }
        if request
            .messages
            .iter()
            .any(|message| message.content == "hang")
        {
            cancellation.cancelled().await;
            anyhow::bail!("cancelled");
        }
        Ok(ModelResponse {
            text: "done".into(),
            ..Default::default()
        })
    }
}
fn harness(store: &Store) -> Harness {
    Harness::new(
        store.clone(),
        Arc::new(Model),
        ToolRegistry::new(),
        HarnessConfig::default(),
    )
}
fn parent_states(
    harness: &Harness,
    parent: &str,
    child: &str,
    cursor: u64,
) -> Result<Vec<HarnessEvent>> {
    Ok(harness.events_after(cursor, Some(parent), 100)?.into_iter().filter(|event| {
        matches!(&event.kind, EventKind::ChildRunStateChanged { child_session_id, .. } if child_session_id == child)
    }).collect())
}

async fn lifecycle_case(store: &Store) -> Result<()> {
    let parent = store.create_session("parent")?.id;
    let child = store
        .create_session_with_parent("child", Default::default(), Some(&parent))?
        .id;
    let hidden = store
        .create_session_with_visibility("worker", Default::default(), Some(&parent), true)?
        .id;
    let harness = harness(store);
    let cursor = harness.last_sequence();
    let before_root = store.storage_stats()?;
    harness
        .prompt(&parent, "complete", RunOptions::default())
        .await?;
    let after_root = store.storage_stats()?;
    assert!(parent_states(&harness, &parent, &parent, cursor)?.is_empty());
    let before_child = store.storage_stats()?;
    let completed = harness
        .prompt(&child, "complete", RunOptions::default())
        .await?;
    let after_child = store.storage_stats()?;
    // Child fanout appends events to the existing source/journal transaction;
    // it introduces no transaction compared with an ordinary parent turn.
    assert_eq!(
        after_child.chat_transaction_count - before_child.chat_transaction_count,
        after_root.chat_transaction_count - before_root.chat_transaction_count
    );
    assert_eq!(
        after_child.serving_transaction_count - before_child.serving_transaction_count,
        after_root.serving_transaction_count - before_root.serving_transaction_count
    );
    let events = parent_states(&harness, &parent, &child, cursor)?;
    assert_eq!(events.len(), 2);
    assert!(
        matches!(&events[0].kind, EventKind::ChildRunStateChanged { state, .. } if state == "running")
    );
    assert!(
        matches!(&events[1].kind, EventKind::ChildRunStateChanged { state, .. } if state == &store.run(&completed.id).unwrap().status)
    );
    for event in &events {
        assert!(event.durable);
        assert_eq!(event.run_id, completed.id);
        // The run id belongs to the flattened envelope exactly once.
        let encoded = serde_json::to_string(event)?;
        assert_eq!(encoded.matches("\"run_id\"").count(), 1);
        let replayed: HarnessEvent = serde_json::from_str(&encoded)?;
        assert_eq!(replayed.run_id, completed.id);
        assert_eq!(
            serde_json::to_value(replayed)?,
            serde_json::to_value(event)?
        );
    }
    harness
        .prompt(&hidden, "complete", RunOptions::default())
        .await?;
    assert!(parent_states(&harness, &parent, &hidden, cursor)?.is_empty());
    assert!(
        !harness.events_after(cursor, Some(&hidden), 100)?.is_empty(),
        "hidden run keeps its own events"
    );
    harness.shutdown(Duration::from_secs(2)).await?;
    Ok(())
}

#[tokio::test]
async fn parent_run_events_share_existing_transactions_and_hide_workers() -> Result<()> {
    lifecycle_case(&Store::in_memory()?).await
}

#[tokio::test]
async fn directory_parent_run_events_share_existing_transactions_and_hide_workers() -> Result<()> {
    let directory = tempfile::tempdir()?;
    lifecycle_case(&Store::open_data_dir(directory.path())?).await
}

#[tokio::test]
async fn parent_terminal_states_match_failed_and_cancelled_rows() -> Result<()> {
    let store = Store::in_memory()?;
    let parent = store.create_session("parent")?.id;
    let harness = harness(&store);
    for (label, text) in [("failed", "fail"), ("cancelled", "hang")] {
        let child = store
            .create_session_with_parent(label, Default::default(), Some(&parent))?
            .id;
        let cursor = harness.last_sequence();
        let run = harness.start(&child, text, RunOptions::default())?;
        if label == "cancelled" {
            assert!(harness.cancel(&child));
        }
        let _ = harness.wait_for_idle(&child).await;
        let stored = store.run(&run.id)?;
        assert_eq!(stored.status, label);
        let events = parent_states(&harness, &parent, &child, cursor)?;
        assert_eq!(events.len(), 2);
        assert!(
            matches!(&events[1].kind, EventKind::ChildRunStateChanged { state, .. } if state == &stored.status)
        );
    }
    harness.shutdown(Duration::from_secs(2)).await?;
    Ok(())
}

#[test]
fn snapshot_preloads_lazy_recovery_before_holding_the_event_lock() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let parent = store.create_session("parent")?.id;
    let child = store
        .create_session_with_parent("child", Default::default(), Some(&parent))?
        .id;
    let path = store.chat_path(&child)?.expect("canonical child database");
    assert!(store.archive_chat(&parent)?);
    assert!(
        !path.exists(),
        "archive evicts the cached canonical connection"
    );

    // Freeze an interrupted record into an unopened archive. No provider or
    // external effect runs; restoring it exercises real lazy-open recovery.
    let archive = std::path::PathBuf::from(format!("{}.zst", path.display()));
    let fixture = directory.path().join("interrupted-fixture.db");
    std::fs::write(
        &fixture,
        zstd::stream::decode_all(std::fs::File::open(&archive)?)?,
    )?;
    let run = crate::Run {
        id: crate::id(),
        session_id: child.clone(),
        model: "scripted-recovery".into(),
        status: "running".into(),
        text: None,
        error: None,
        started_at: crate::now(),
        ended_at: None,
        usage: None,
    };
    {
        let connection = rusqlite::Connection::open(&fixture)?;
        connection.execute(
            "INSERT INTO runs(id,session_id,status,data) VALUES(?1,?2,'running',?3)",
            params![run.id, child, serde_json::to_string(&run)?],
        )?;
        connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    }
    std::fs::write(
        &archive,
        zstd::stream::encode_all(std::fs::File::open(&fixture)?, 9)?,
    )?;

    let harness = harness(&store);
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = {
        let (harness, store, child) = (harness.clone(), store.clone(), child.clone());
        std::thread::spawn(move || {
            let result = harness.snapshot_with(&child, || store.runs(&child));
            let _ = sender.send(result);
        })
    };
    let (runs, position) = receiver
        .recv_timeout(Duration::from_secs(5))
        .context("snapshot deadlocked while lazy recovery published a parent event")??;
    worker
        .join()
        .map_err(|_| anyhow::anyhow!("snapshot worker panicked"))?;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, run.id);
    assert_eq!(runs[0].status, "interrupted");
    let events = parent_states(&harness, &parent, &child, 0)?;
    assert_eq!(events.len(), 1);
    assert!(events[0].durable);
    assert!(events[0].sequence <= position.seq);
    assert!(
        matches!(&events[0].kind, EventKind::ChildRunStateChanged { state, .. } if state == "interrupted")
    );
    Ok(())
}
