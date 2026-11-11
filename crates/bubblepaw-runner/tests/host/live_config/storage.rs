//! Storage overhead of one incremental config notice.
use super::*;

#[tokio::test]
async fn diff_notice_adds_exactly_one_message_and_one_write_transaction() {
    let fixture = Fixture::new(&config("")).await;
    fixture.turn("warm").await;
    // Physical WAL length can stay flat while SQLite reuses checkpointed pages.
    // Start both samples from an empty WAL to compare actual per-turn growth.
    fixture.store.idle_checkpoint().unwrap();
    let before = fixture.store.storage_stats().unwrap();
    fixture.turn("baseline").await;
    let baseline = fixture.store.storage_stats().unwrap();
    let prefix_key = format!("tool_surface:{}", fixture.session);
    let prefix_before = fixture
        .store
        .chat_metadata(&fixture.session, &prefix_key)
        .unwrap();
    fixture.store.idle_checkpoint().unwrap();
    let changed_start = fixture.store.storage_stats().unwrap();
    fixture.write(&config("[exec]\nsync_timeout_secs=7\n"));
    // Reload itself stays entirely in memory.
    let report = fixture.live.reload(None).await;
    assert!(!report.changed.is_empty());
    let reloaded = fixture.store.storage_stats().unwrap();
    assert_eq!(reloaded.transaction_count, changed_start.transaction_count);
    assert_eq!(reloaded.wal_bytes, changed_start.wal_bytes);
    fixture.turn("with diff").await;
    let changed = fixture.store.storage_stats().unwrap();
    let prefix_after = fixture
        .store
        .chat_metadata(&fixture.session, &prefix_key)
        .unwrap();
    assert!(
        prefix_before == prefix_after,
        "config reload must preserve the chat prefix"
    );
    println!(
        "diff-only overhead: messages={} transactions={}; notice={} bytes; baseline turn WAL={}, changed turn WAL={}",
        (changed.message_count - changed_start.message_count)
            - (baseline.message_count - before.message_count),
        (changed.transaction_count - changed_start.transaction_count)
            - (baseline.transaction_count - before.transaction_count),
        fixture.notices()[0].len(),
        baseline.wal_bytes.saturating_sub(before.wal_bytes),
        changed.wal_bytes.saturating_sub(changed_start.wal_bytes)
    );
    assert_eq!(
        changed.message_count - changed_start.message_count,
        baseline.message_count - before.message_count + 1
    );
    assert_eq!(
        changed.transaction_count - changed_start.transaction_count,
        baseline.transaction_count - before.transaction_count + 1
    );
    assert_eq!(fixture.notices().len(), 1);
    assert!(
        changed.wal_bytes.saturating_sub(changed_start.wal_bytes)
            <= baseline.wal_bytes.saturating_sub(before.wal_bytes) + 65536
    );
    fixture.shutdown().await;
}
