use anyhow::Result;
use bubblepaw_core::{CHECKPOINT_PREFIX, CheckpointRecord, Store};
use rusqlite::{Connection, params};

fn checkpoint_id(name: &str) -> String {
    format!("{CHECKPOINT_PREFIX}{name}")
}

fn record(id: &str, patch_index: u64, boundary_seq: i64) -> CheckpointRecord<'_> {
    CheckpointRecord {
        id,
        patch_index,
        boundary_seq,
        metadata: "{}",
    }
}

fn connection(store: &Store, session: &str) -> Result<Connection> {
    Ok(Connection::open(store.chat_path(session)?.unwrap())?)
}

fn count(store: &Store, session: &str, sql: &str) -> Result<i64> {
    Ok(connection(store, session)?.query_row(sql, [], |row| row.get(0))?)
}

fn preimage_rows(store: &Store, session: &str) -> Result<i64> {
    count(store, session, "SELECT COUNT(*) FROM checkpoint_blobs")
}

fn plain_blob_rows(store: &Store, session: &str) -> Result<i64> {
    count(store, session, "SELECT COUNT(*) FROM blobs")
}

fn record_rows(store: &Store, session: &str) -> Result<i64> {
    count(store, session, "SELECT COUNT(*) FROM checkpoint_records")
}

fn legacy_rows(store: &Store, session: &str) -> Result<i64> {
    count(
        store,
        session,
        "SELECT COUNT(*) FROM attachments WHERE substr(id,1,length('file-checkpoint-v1/'))='file-checkpoint-v1/'",
    )
}

/// Insert a row under the checkpoint prefix that names a plain blob, as an older
/// build wrote it. Store APIs refuse prefixed attachment IDs now, so this uses raw SQL.
fn insert_legacy_checkpoint_row(
    store: &Store,
    session: &str,
    id: &str,
    hash: &str,
    metadata: &str,
) -> Result<()> {
    connection(store, session)?.execute(
        "INSERT INTO attachments(session_id,id,metadata,blob_hash) VALUES(?1,?2,?3,?4)",
        params![session, id, metadata, hash],
    )?;
    Ok(())
}

#[test]
fn plain_blob_equal_to_checkpoint_preimage_is_independent_in_both_orders() -> Result<()> {
    let bytes: &[u8] = b"shared bytes";
    // Plain bytes that spell an old domain tag and separator before the same bytes.
    let tagged = [b"bubblepaw-checkpoint-v1\0".as_slice(), bytes].concat();
    for plain_first in [true, false] {
        let directory = tempfile::tempdir()?;
        let store = Store::open_data_dir(directory.path())?;
        let session = store.create_session("independent")?.id;
        let (plain_hash, checkpoint_hash) = if plain_first {
            let plain = store.store_attachment(&session, "plain", "{}", bytes)?;
            let checkpoint =
                store.write_checkpoint(&session, record(&checkpoint_id("c"), 1, 0), bytes, &[])?;
            (plain, checkpoint)
        } else {
            let checkpoint =
                store.write_checkpoint(&session, record(&checkpoint_id("c"), 1, 0), bytes, &[])?;
            let plain = store.store_attachment(&session, "plain", "{}", bytes)?;
            (plain, checkpoint)
        };
        store.store_attachment(&session, "tagged", "{}", &tagged)?;
        // Content hashes are plain SHA-256, so both rows name the same hash.
        assert_eq!(plain_hash, checkpoint_hash);
        assert_eq!(store.attachment(&session, "plain")?.unwrap().1, bytes);
        assert_eq!(
            store
                .checkpoint_preimage(&session, &checkpoint_id("c"))?
                .unwrap(),
            bytes
        );
        assert_eq!(store.attachment(&session, "tagged")?.unwrap().1, tagged);

        // Pruning the checkpoint record removes only its preimage, never the plain blob.
        let prunes = [(session.clone(), 1)];
        store.write_checkpoint(
            &session,
            record(&checkpoint_id("next"), 2, 2),
            b"next",
            &prunes,
        )?;
        assert_eq!(preimage_rows(&store, &session)?, 1);
        assert_eq!(store.attachment(&session, "plain")?.unwrap().1, bytes);
        assert_eq!(store.attachment(&session, "tagged")?.unwrap().1, tagged);
        // Plain blobs only: "plain" and "tagged". The preimage lives in checkpoint_blobs.
        assert_eq!(plain_blob_rows(&store, &session)?, 2);
    }
    Ok(())
}

#[test]
fn checkpoint_write_prunes_records_below_bound_and_only_unreferenced_preimages() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let session = store.create_session("prune")?.id;
    let shared = store.write_checkpoint(
        &session,
        record(&checkpoint_id("a"), 1, 1),
        b"shared bytes",
        &[],
    )?;
    store.write_checkpoint(
        &session,
        record(&checkpoint_id("b"), 2, 3),
        b"shared bytes",
        &[],
    )?;
    let gone = store.write_checkpoint(
        &session,
        record(&checkpoint_id("c"), 3, 1),
        b"only in c",
        &[],
    )?;
    assert_eq!(record_rows(&store, &session)?, 3);
    assert_eq!(preimage_rows(&store, &session)?, 2);

    // Bound 2 keeps boundary 3 (b) and removes boundary 1 (a and c).
    let prunes = [(session.clone(), 2)];
    let new = store.write_checkpoint(
        &session,
        record(&checkpoint_id("d"), 4, 4),
        b"new bytes",
        &prunes,
    )?;
    assert_eq!(
        store.checkpoint_preimage(&session, &checkpoint_id("a"))?,
        None
    );
    assert_eq!(
        store.checkpoint_preimage(&session, &checkpoint_id("c"))?,
        None
    );
    assert_eq!(
        store
            .checkpoint_preimage(&session, &checkpoint_id("b"))?
            .unwrap(),
        b"shared bytes"
    );
    assert_eq!(
        store
            .checkpoint_preimage(&session, &checkpoint_id("d"))?
            .unwrap(),
        b"new bytes"
    );
    assert_eq!(record_rows(&store, &session)?, 2);
    // The preimage named by a surviving record stays, and the orphaned one goes.
    assert_eq!(preimage_rows(&store, &session)?, 2);
    let named = count(
        &store,
        &session,
        &format!("SELECT COUNT(*) FROM checkpoint_blobs WHERE hash IN ('{shared}','{new}')"),
    )?;
    assert_eq!(named, 2);
    let dropped = count(
        &store,
        &session,
        &format!("SELECT COUNT(*) FROM checkpoint_blobs WHERE hash='{gone}'"),
    )?;
    assert_eq!(dropped, 0);
    Ok(())
}

#[test]
fn delayed_prune_cannot_delete_a_checkpoint_written_after_its_bound_was_read() -> Result<()> {
    // Three sessions share one chat. S holds a stale checkpoint (patch 1, boundary 1).
    // T reads the prune bound (keep from 5) and plans to prune S. U writes first and
    // removes S's stale record. S then writes its next checkpoint, which must not
    // reuse patch 1. T's write commits last with the bound it read earlier.
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let root = store.create_session("root")?.id;
    let s = store
        .create_session_with_parent("s", Default::default(), Some(&root))?
        .id;
    let t = store
        .create_session_with_parent("t", Default::default(), Some(&root))?
        .id;
    let u = store
        .create_session_with_parent("u", Default::default(), Some(&root))?
        .id;
    store.write_checkpoint(&s, record(&checkpoint_id("s/1"), 1, 1), b"stale", &[])?;

    // T read its plan before U and S acted: S's records below 5 are stale.
    let planned = [(s.clone(), 5)];

    // U prunes S's stale record with its own write.
    store.write_checkpoint(
        &u,
        record(&checkpoint_id("u/1"), 1, 6),
        b"u",
        &[(s.clone(), 5)],
    )?;
    assert_eq!(store.checkpoint_preimage(&s, &checkpoint_id("s/1"))?, None);

    // S numbers from its high-water mark, so the new record is patch 2, not 1.
    assert_eq!(store.checkpoint_high_water(&s)?, 1);
    store.write_checkpoint(&s, record(&checkpoint_id("s/2"), 2, 6), b"fresh", &[])?;
    assert_eq!(store.checkpoint_high_water(&s)?, 2);

    // A write that reuses patch 1 is refused.
    assert!(
        store
            .write_checkpoint(&s, record(&checkpoint_id("s/1-again"), 1, 6), b"x", &[])
            .is_err()
    );

    // T's late write applies its stale bound. S's new record sits at boundary 6, so it survives.
    store.write_checkpoint(&t, record(&checkpoint_id("t/1"), 1, 7), b"t", &planned)?;
    assert_eq!(
        store
            .checkpoint_preimage(&s, &checkpoint_id("s/2"))?
            .unwrap(),
        b"fresh"
    );
    Ok(())
}

#[test]
fn high_water_mark_survives_a_full_prune_and_blocks_reuse() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let session = store.create_session("numbering")?.id;
    store.write_checkpoint(&session, record(&checkpoint_id("1"), 1, 1), b"one", &[])?;
    // A bound above every record empties the session.
    store.write_checkpoint(
        &session,
        record(&checkpoint_id("2"), 2, 2),
        b"two",
        &[(session.clone(), 100)],
    )?;
    assert_eq!(
        store.checkpoint_preimage(&session, &checkpoint_id("1"))?,
        None
    );
    assert_eq!(store.checkpoint_high_water(&session)?, 2);
    assert_eq!(record_rows(&store, &session)?, 1);
    // The new write counts from the mark. Patch 1 is refused even though its record is gone.
    assert!(
        store
            .write_checkpoint(&session, record(&checkpoint_id("1-again"), 1, 3), b"x", &[])
            .is_err()
    );
    store.write_checkpoint(&session, record(&checkpoint_id("3"), 3, 3), b"three", &[])?;
    assert_eq!(store.checkpoint_high_water(&session)?, 3);
    Ok(())
}

#[test]
fn prune_across_sessions_shares_one_write_transaction() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let root = store.create_session("root")?.id;
    let child = store
        .create_session_with_parent("child", Default::default(), Some(&root))?
        .id;
    store.write_checkpoint(&root, record(&checkpoint_id("r"), 1, 1), b"root bytes", &[])?;
    store.write_checkpoint(
        &child,
        record(&checkpoint_id("c"), 1, 1),
        b"child bytes",
        &[],
    )?;
    let before = store.revision();
    let prunes = [(root.clone(), 5), (child.clone(), 5)];
    store.write_checkpoint(
        &root,
        record(&checkpoint_id("next"), 2, 6),
        b"next bytes",
        &prunes,
    )?;
    assert_eq!(store.revision(), before + 1);
    assert_eq!(store.checkpoint_preimage(&root, &checkpoint_id("r"))?, None);
    assert_eq!(
        store.checkpoint_preimage(&child, &checkpoint_id("c"))?,
        None
    );
    Ok(())
}

#[test]
fn checkpoint_api_never_touches_plain_attachments() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let session = store.create_session("guards")?.id;
    // Attachment IDs under the checkpoint prefix are refused by the attachment API.
    assert!(
        store
            .store_attachment(&session, &checkpoint_id("x"), "{}", b"x")
            .is_err()
    );
    store.store_attachment(&session, "plain", "{}", b"kept")?;
    // A prune bound that covers everything leaves the plain attachment alone.
    let prunes = [(session.clone(), i64::MAX)];
    store.write_checkpoint(&session, record(&checkpoint_id("y"), 1, 1), b"y", &prunes)?;
    store.sweep_legacy_checkpoints(&session, ("marker", "v2"), &[], &[])?;
    assert_eq!(store.attachment(&session, "plain")?.unwrap().1, b"kept");
    assert_eq!(record_rows(&store, &session)?, 1);
    Ok(())
}

#[test]
fn sweep_copies_current_and_previous_turn_and_removes_legacy_rows() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let session = store.create_session("sweep")?.id;
    let legacy_blob = store.store_attachment(&session, "holder", "{}", b"legacy bytes")?;
    // Patch 1 sits at boundary 1, below the keep bound 3: an old turn, dropped.
    insert_legacy_checkpoint_row(
        &store,
        &session,
        &checkpoint_id("legacy/1"),
        &legacy_blob,
        r#"{"patch_index":1,"boundary_seq":1}"#,
    )?;
    // Patch 2 sits at boundary 5, at or above the bound: the previous or current turn, kept.
    insert_legacy_checkpoint_row(
        &store,
        &session,
        &checkpoint_id("legacy/2"),
        &legacy_blob,
        r#"{"patch_index":2,"boundary_seq":5}"#,
    )?;
    assert_eq!(legacy_rows(&store, &session)?, 2);

    let before = store.revision();
    store.sweep_legacy_checkpoints(&session, ("marker", "v2"), &[(session.clone(), 3)], &[])?;
    // One transaction; legacy rows go, the kept record moves to the current tables.
    assert_eq!(store.revision(), before + 1);
    assert_eq!(legacy_rows(&store, &session)?, 0);
    assert_eq!(record_rows(&store, &session)?, 1);
    assert_eq!(preimage_rows(&store, &session)?, 1);
    assert_eq!(
        store
            .checkpoint_preimage(&session, &checkpoint_id("legacy/2"))?
            .unwrap(),
        b"legacy bytes"
    );
    assert_eq!(
        store.checkpoint_preimage(&session, &checkpoint_id("legacy/1"))?,
        None
    );
    // blobs is never changed by the sweep; the holder row and its blob remain.
    assert_eq!(plain_blob_rows(&store, &session)?, 1);
    assert_eq!(
        store.attachment(&session, "holder")?.unwrap().1,
        b"legacy bytes"
    );
    assert_eq!(
        store.chat_metadata(&session, "marker")?.as_deref(),
        Some("v2")
    );
    // The sweep raised the high-water mark to the highest legacy patch.
    assert_eq!(store.checkpoint_high_water(&session)?, 2);
    assert!(
        store
            .write_checkpoint(&session, record(&checkpoint_id("stale"), 1, 6), b"x", &[])
            .is_err()
    );
    Ok(())
}

#[test]
fn checkpoint_preimage_lookup_uses_the_preimage_index() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let session = store.create_session("plan")?.id;
    store.write_checkpoint(&session, record(&checkpoint_id("a"), 1, 1), b"a", &[])?;
    let conn = connection(&store, &session)?;
    // Same statement as the reference check in store_layout.rs.
    let plan: Vec<String> = conn
        .prepare(
            "EXPLAIN QUERY PLAN SELECT EXISTS(SELECT 1 FROM checkpoint_records WHERE preimage_hash=?1)",
        )?
        .query_map(params!["x"], |row| row.get::<_, String>(3))?
        .collect::<rusqlite::Result<_>>()?;
    assert!(
        plan.iter()
            .any(|line| line.contains("checkpoint_records_preimage")),
        "plan does not use the index: {plan:?}"
    );
    Ok(())
}

/// A second caller for a chat must wait while that chat's open hook runs. Otherwise it
/// could receive the chat before its migration has been attempted.
#[test]
fn chat_is_not_visible_to_other_callers_until_its_open_hook_has_run() -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;

    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let session = store.create_session("ordering")?.id;
    assert!(store.archive_chat(&session)?);

    let armed = Arc::new(AtomicBool::new(false));
    let fired = Arc::new(AtomicBool::new(false));
    let migrated = Arc::new(AtomicBool::new(false));
    let blocked = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let done_rx = Mutex::new(done_rx);
    let (seen_tx, seen_rx) = mpsc::channel::<bool>();
    let seen_tx = Mutex::new(seen_tx);
    let directory_handle = store.clone();
    let hook_armed = armed.clone();
    let (hook_fired, hook_migrated, hook_blocked) =
        (fired.clone(), migrated.clone(), blocked.clone());
    store.on_chat_open(
        "test.ordering",
        Arc::new(move |chat: &Store| {
            if !hook_armed.load(Ordering::SeqCst) {
                return Ok(());
            }
            hook_fired.store(true, Ordering::SeqCst);
            let session = chat.chat_session_ids()?.remove(0);
            // A second caller asks for the same chat while the hook runs.
            let caller = directory_handle.clone();
            let caller_migrated = hook_migrated.clone();
            let caller_done = done_tx.clone();
            let caller_seen = seen_tx.lock().unwrap().clone();
            std::thread::spawn(move || {
                let _ = caller.checkpoint_records_page(&session, None, 10);
                let _ = caller_seen.send(caller_migrated.load(Ordering::SeqCst));
                let _ = caller_done.send(());
            });
            let waiting = done_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_millis(300));
            hook_blocked.store(waiting.is_err(), Ordering::SeqCst);
            // The migration is done here; only now may the second caller proceed.
            hook_migrated.store(true, Ordering::SeqCst);
            Ok(())
        }),
    )?;

    armed.store(true, Ordering::SeqCst);
    store.checkpoint_records_page(&session, None, 10)?;
    armed.store(false, Ordering::SeqCst);

    assert!(
        fired.load(Ordering::SeqCst),
        "the open did not run the hook"
    );
    assert!(
        blocked.load(Ordering::SeqCst),
        "a second caller received the chat while its open hook was running"
    );
    assert!(seen_rx.recv_timeout(Duration::from_secs(10))?);
    Ok(())
}
