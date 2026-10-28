use super::*;
use crate::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, TextCallback,
    ToolRegistry,
};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

struct Idle;
#[async_trait]
impl Provider for Idle {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(ModelResponse::default())
    }
}
fn harness(store: &Store) -> Harness {
    Harness::new(
        store.clone(),
        Arc::new(Idle),
        ToolRegistry::new(),
        HarnessConfig::default(),
    )
}

fn assert_flushed_event_count(store: &Store, cursor: u64, expected: u64) -> Result<()> {
    // The serving counter measures buffer COMMITs, not each journal operation.
    // Verify its externally readable journal after the existing explicit flush.
    store.flush_serving()?;
    if let Some(directory) = store.data_dir() {
        let connection = rusqlite::Connection::open_with_flags(
            directory.join("serving.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM events WHERE seq>?1",
            [i64::try_from(cursor)?],
            |row| row.get(0),
        )?;
        assert_eq!(u64::try_from(count)?, expected);
    }
    Ok(())
}

fn shared_projection_case(store: &Store, directory: bool) -> Result<()> {
    let parent = store.create_session("projection target")?.id;
    let child = store
        .create_session_with_parent("source", Default::default(), Some(&parent))?
        .id;
    if directory {
        assert_eq!(store.chat_path(&child)?, store.chat_path(&parent)?);
    }
    let harness = harness(store);
    let cursor = harness.last_sequence();
    let before = store.storage_stats()?;
    store.update_host_state_with_projection(
        &child,
        "run",
        "source",
        (&parent, "projection"),
        |_| {
            Ok((
                json!({"status":"pending"}),
                json!({"source":child,"status":"pending"}),
            ))
        },
    )?;
    let after = store.storage_stats()?;
    assert_eq!(
        after.chat_transaction_count - before.chat_transaction_count,
        1
    );
    let source = harness.events_after(cursor, Some(&child), 10)?;
    let target = harness.events_after(cursor, Some(&parent), 10)?;
    assert_eq!(source.len(), 1);
    assert_eq!(target.len(), 1);
    assert!(source[0].durable && target[0].durable);
    assert_eq!(target[0].sequence, source[0].sequence + 1);
    assert_eq!(target[0].run_id, "run");
    assert_eq!(
        store.host_state(&child, "source")?,
        Some(json!({"status":"pending"}))
    );
    assert_eq!(
        store.host_state(&parent, "projection")?,
        Some(json!({"source":child,"status":"pending"}))
    );
    assert_flushed_event_count(store, cursor, 2)?;

    // The parent row is a real part of the source transaction. A target failure
    // cannot leave a new child value or either event visible.
    let chat = store.scoped(&child)?.unwrap_or_else(|| store.clone());
    chat.read(|tx| {
        tx.execute_batch("CREATE TEMP TRIGGER reject_projection BEFORE INSERT ON host_state WHEN NEW.key='projection' BEGIN SELECT RAISE(ABORT,'projection rejected'); END")?;
        Ok(())
    })?;
    let cursor = harness.last_sequence();
    let before = store.storage_stats()?;
    assert!(
        store
            .update_host_state_with_projection(
                &child,
                "run",
                "source",
                (&parent, "projection"),
                |_| {
                    Ok((
                        json!({"status":"answered"}),
                        json!({"source":child,"status":"answered"}),
                    ))
                }
            )
            .is_err()
    );
    assert_eq!(
        store.storage_stats()?.chat_transaction_count,
        before.chat_transaction_count
    );
    assert_eq!(harness.last_sequence(), cursor);
    assert!(harness.events_after(cursor, None, 10)?.is_empty());
    assert_eq!(
        store.host_state(&child, "source")?,
        Some(json!({"status":"pending"}))
    );
    assert_eq!(
        store.host_state(&parent, "projection")?,
        Some(json!({"source":child,"status":"pending"}))
    );
    Ok(())
}

#[test]
fn companion_state_and_events_share_one_in_memory_transaction() -> Result<()> {
    shared_projection_case(&Store::in_memory()?, false)
}

#[test]
fn companion_state_and_events_share_one_directory_chat_transaction() -> Result<()> {
    let directory = tempfile::tempdir()?;
    shared_projection_case(&Store::open_data_dir(directory.path())?, true)
}

#[test]
fn cross_database_companion_uses_one_additional_transaction() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let source = store.create_session("source root")?.id;
    let target = store.create_session("different root")?.id;
    assert_ne!(store.chat_path(&source)?, store.chat_path(&target)?);
    let harness = harness(&store);
    let cursor = harness.last_sequence();
    let before = store.storage_stats()?;
    store.update_host_state_with_projection(
        &source,
        "run",
        "source",
        (&target, "projection"),
        |_| Ok((json!(1), json!({"source":source,"value":1}))),
    )?;
    let after = store.storage_stats()?;
    assert_eq!(
        after.chat_transaction_count - before.chat_transaction_count,
        2
    );
    assert_eq!(harness.events_after(cursor, Some(&source), 10)?.len(), 1);
    assert_eq!(harness.events_after(cursor, Some(&target), 10)?.len(), 1);
    assert_flushed_event_count(&store, cursor, 2)?;
    Ok(())
}
