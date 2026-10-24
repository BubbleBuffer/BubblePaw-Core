mod support;

use anyhow::Result;
use bubblepaw_core::{ContextStrategy, Role, Store};
use rusqlite::Connection;
use support::stored_text;

#[test]
fn contentless_search_resolves_rows_and_filters_roots() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let root = store.create_session("one")?;
    let child = store.create_session_with_parent("child", ContextStrategy::Lcm, Some(&root.id))?;
    let other = store.create_session("two")?;
    store.append_batch(&[
        stored_text(&root.id, "r", "cobalt telescope outside"),
        stored_text(&child.id, "c", "cobalt satellite inside"),
        stored_text(&other.id, "o", "cobalt telescope unrelated"),
    ])?;
    let hits = store.search_messages("cobalt", Some(&root.id), 10)?;
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|hit| hit.root_id == root.id));
    assert!(
        hits.iter()
            .any(|hit| hit.session_id == child.id && hit.snippet.contains("satellite"))
    );
    assert_eq!(
        store
            .search_messages("\"cobalt telescope\"", None, 10)?
            .len(),
        2
    );
    store.flush_serving()?;
    let serving = Connection::open(dir.path().join("serving.db"))?;
    let sql: String = serving.query_row(
        "SELECT sql FROM sqlite_master WHERE name='message_search'",
        [],
        |row| row.get(0),
    )?;
    assert!(sql.contains("contentless_delete=1"));
    let nonempty: i64 = serving.query_row(
        "SELECT COUNT(*) FROM message_search WHERE body IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(nonempty, 0);
    Ok(())
}

#[test]
fn wipe_rebuild_is_bounded_and_deletion_removes_terms() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("search rebuild")?;
    let messages: Vec<_> = (0..7)
        .map(|i| stored_text(&session.id, &format!("m{i}"), "amethyst observatory"))
        .collect();
    store.append_batch(&messages)?;
    assert_eq!(store.search_messages("amethyst", None, 100)?.len(), 7);
    store.serving_wipe()?;
    assert!(store.search_messages("amethyst", None, 100)?.is_empty());
    let first = store.reindex_search(None, 2)?;
    assert_eq!(first.indexed_messages, 2);
    assert!(!first.complete);
    let second = store.reindex_search(None, 2)?;
    assert_eq!(second.indexed_messages, 2);
    assert!(!second.complete);
    assert_eq!(store.search_messages("amethyst", None, 100)?.len(), 4);
    assert!(store.reindex_search(None, 10)?.complete);
    store.delete_session(&session.id)?;
    assert!(store.search_messages("amethyst", None, 100)?.is_empty());
    Ok(())
}

#[test]
fn bounded_reindex_flushes_one_page_and_progress_once_per_successful_call() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("visible search pages")?;
    let messages: Vec<_> = (0..5)
        .map(|index| {
            stored_text(
                &session.id,
                &format!("page-{index}"),
                "malachite page record",
            )
        })
        .collect();
    store.append_batch(&messages)?;
    store.serving_wipe()?;
    store.flush_serving()?;
    let foreign = Connection::open_with_flags(
        dir.path().join("serving.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    for (rows, complete) in [(2, false), (4, false), (5, true)] {
        let before = store.storage_stats()?;
        let progress = store.reindex_search(None, 2)?;
        assert_eq!(progress.complete, complete);
        let after = store.storage_stats()?;
        assert_eq!(
            after.serving_transaction_count - before.serving_transaction_count,
            1
        );
        assert_eq!(after.transaction_count - before.transaction_count, 1);
        let (through, persisted_complete): (i64, bool) = foreign.query_row(
            "SELECT through_seq,complete FROM search_progress WHERE session_id=?1",
            [&session.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(
            through,
            store.message_seq(&session.id, &messages[rows - 1].id)?
        );
        assert_eq!(persisted_complete, complete);
        assert_eq!(
            foreign.query_row("SELECT COUNT(*) FROM search_rows", [], |row| row
                .get::<_, i64>(0))?,
            i64::try_from(rows)?
        );
        assert_eq!(
            foreign.query_row(
                "SELECT COUNT(*) FROM message_search WHERE message_search MATCH 'malachite'",
                [],
                |row| row.get::<_, i64>(0)
            )?,
            i64::try_from(rows)?
        );
    }
    let before = store.storage_stats()?.transaction_count;
    assert_eq!(store.reindex_search(None, 2)?.indexed_messages, 0);
    assert_eq!(store.storage_stats()?.transaction_count, before);
    Ok(())
}

#[test]
fn indexed_query_decodes_only_hits() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("only hits")?;
    store.append_batch(&[
        stored_text(&session.id, "good", "vermilion nebula"),
        stored_text(&session.id, "bad", "ordinary clouds"),
    ])?;
    let path = store.chat_path(&session.id)?.unwrap();
    Connection::open(path)?.execute(
        "UPDATE messages SET data='intentionally invalid body' WHERE message_id='bad'",
        [],
    )?;
    let hits = store.search_messages("vermilion", None, 10)?;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].message_id, "good");
    // A complete index also avoids a cold-history walk during reindex calls.
    assert_eq!(store.reindex_search(None, 100)?.indexed_messages, 0);
    Ok(())
}

#[test]
fn search_call_arguments_have_canonical_snippets() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("call terms")?;
    let mut call = stored_text(&session.id, "call", "");
    call.role = Role::Assistant;
    call.tool_calls = vec![bubblepaw_core::ToolCall {
        id: "call-id".into(),
        name: "look_up".into(),
        arguments: serde_json::json!({"object":"violet 雪景 observatory"}),
    }];
    store.append_batch(&[call])?;
    let hits = store.search_messages("violet", None, 10)?;
    assert_eq!(hits.len(), 1);
    assert!(hits[0].snippet.contains("violet 雪景"));
    assert_eq!(store.search_messages("look_up", None, 10)?.len(), 1);
    Ok(())
}

#[test]
fn rejected_index_write_cannot_roll_back_record_and_is_reindexable() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("index failure")?;
    store.append_batch(&[stored_text(&session.id, "before", "indigo panorama")])?;
    store.flush_serving()?;
    let serving = Connection::open(dir.path().join("serving.db"))?;
    serving.execute_batch("CREATE TRIGGER reject_search BEFORE INSERT ON search_rows BEGIN SELECT RAISE(ABORT,'synthetic index failure'); END;")?;
    store.append_batch(&[stored_text(&session.id, "accepted", "amber panorama")])?;
    assert_eq!(store.messages(&session.id)?.len(), 2);
    assert!(store.search_messages("amber", None, 10)?.is_empty());
    store.flush_serving()?;
    serving.execute_batch("DROP TRIGGER reject_search")?;
    let progress = store.reindex_search(None, 20)?;
    assert!(progress.complete);
    assert_eq!(store.search_messages("amber", None, 10)?.len(), 1);
    Ok(())
}

#[test]
fn startup_reconciliation_removes_search_rows_for_a_missing_chat() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path;
    {
        let store = Store::open_data_dir(dir.path())?;
        let session = store.create_session("interrupted deletion")?;
        store.append_batch(&[stored_text(&session.id, "orphan", "turquoise panorama")])?;
        store.flush_serving()?;
        path = store.chat_path(&session.id)?.unwrap();
    }
    // Model interruption after the canonical root file was removed but before
    // projection deletion. Startup must remove dangling FTS references.
    std::fs::remove_file(path)?;
    let store = Store::open_data_dir(dir.path())?;
    assert!(store.search_messages("turquoise", None, 10)?.is_empty());
    Ok(())
}

#[test]
#[ignore = "subprocess crash fixture; invoked by its parent test"]
fn abrupt_search_child() -> Result<()> {
    let directory = std::path::PathBuf::from(std::env::var("BUBBLEPAW_SEARCH_CRASH_DIR")?);
    let store = Store::open_data_dir(&directory)?;
    let session = store.create_session("search crash")?;
    store.append_batch(&[stored_text(&session.id, "old", "quartz old record")])?;
    store.flush_serving()?;
    if std::env::var_os("BUBBLEPAW_SEARCH_ARCHIVE").is_some() {
        Connection::open(directory.join("serving.db"))?.execute(
            "VACUUM INTO ?1",
            [directory.join("old-serving.db").to_string_lossy().as_ref()],
        )?;
    }
    store.append_batch(&[
        stored_text(&session.id, "new-1", "topaz new record"),
        stored_text(&session.id, "new-2", "topaz second record"),
    ])?;
    if std::env::var_os("BUBBLEPAW_SEARCH_ARCHIVE").is_some() {
        assert!(store.archive_chat(&session.id)?);
    }
    std::fs::write(directory.join("ready.part"), session.id)?;
    std::fs::rename(directory.join("ready.part"), directory.join("ready"))?;
    loop {
        std::thread::park();
    }
}

fn kill_search_child(directory: &std::path::Path, archive: bool) -> Result<String> {
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .args(["--exact", "abrupt_search_child", "--ignored"])
        .env("BUBBLEPAW_SEARCH_CRASH_DIR", directory)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if archive {
        command.env("BUBBLEPAW_SEARCH_ARCHIVE", "1");
    }
    let mut child = command.spawn()?;
    let ready = directory.join("ready");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !ready.is_file() && std::time::Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            anyhow::bail!("Search crash fixture exited before readiness");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    child.kill()?;
    child.wait()?;
    anyhow::ensure!(ready.is_file(), "Search crash fixture timed out");
    Ok(std::fs::read_to_string(ready)?)
}

#[test]
fn abrupt_full_append_repairs_old_complete_search_progress_before_later_appends() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let session = kill_search_child(dir.path(), false)?;
    let store = Store::open_data_dir(dir.path())?;
    assert_eq!(store.messages(&session)?.len(), 3);
    assert!(store.search_messages("topaz", None, 10)?.is_empty());
    store.append_batch(&[stored_text(&session, "new-3", "topaz later record")])?;
    for index in 0..3 {
        let progress = store.reindex_search(None, 1)?;
        assert_eq!(progress.indexed_messages, 1);
        assert_eq!(progress.complete, index == 2);
    }
    assert_eq!(store.search_messages("quartz", None, 10)?.len(), 1);
    assert_eq!(store.search_messages("topaz", None, 10)?.len(), 3);
    assert_eq!(store.reindex_search(None, 1)?.indexed_messages, 0);
    Ok(())
}

#[test]
fn archived_target_detects_lost_normal_index_without_unpacking_complete_archives() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let session = kill_search_child(dir.path(), true)?;
    let chat = dir.path().join("chats").join(format!("{session}.db"));
    assert!(!chat.exists());
    // Model a NORMAL projection rollback while FULL record/catalog commits survive.
    for suffix in ["", "-wal", "-shm"] {
        let file = dir.path().join(format!("serving.db{suffix}"));
        if file.exists() {
            std::fs::remove_file(file)?;
        }
    }
    std::fs::copy(
        dir.path().join("old-serving.db"),
        dir.path().join("serving.db"),
    )?;
    let store = Store::open_data_dir(dir.path())?;
    assert!(!chat.exists(), "Startup must retain a known archive packed");
    assert!(!store.reindex_search(None, 0)?.complete);
    assert!(
        !chat.exists(),
        "Zero-budget reindex cannot unpack the missing tail"
    );
    assert!(store.search_messages("topaz", None, 10)?.is_empty());
    let first = store.reindex_search(None, 1)?;
    assert_eq!(first.indexed_messages, 1);
    assert!(!first.complete);
    assert!(store.reindex_search(None, 1)?.complete);
    assert_eq!(store.search_messages("topaz", None, 10)?.len(), 2);
    assert_eq!(store.search_messages("quartz", None, 10)?.len(), 1);
    assert!(store.archive_chat(&session)?);
    assert_eq!(store.reindex_search(None, 1)?.indexed_messages, 0);
    assert!(
        !chat.exists(),
        "A repaired complete index must keep its archive packed"
    );
    Ok(())
}

#[test]
fn rowwise_reindex_retries_a_failed_page_without_duplicate_terms() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("rowwise repair")?;
    let messages = [
        stored_text(&session.id, "one", "lapis first"),
        stored_text(&session.id, "two", "sienna second"),
        stored_text(&session.id, "three", "scarlet third"),
    ];
    store.append_batch(&messages)?;
    store.serving_wipe()?;
    assert_eq!(store.reindex_search(None, 1)?.indexed_messages, 1);
    let canonical = Connection::open(store.chat_path(&session.id)?.unwrap())?;
    canonical.execute(
        "UPDATE messages SET data='invalid body' WHERE message_id='three'",
        [],
    )?;
    assert!(store.reindex_search(None, 2).is_err());
    assert_eq!(store.search_messages("lapis", None, 10)?.len(), 1);
    assert_eq!(
        store.search_messages("sienna", None, 10)?.len(),
        1,
        "Completed buffered rows survive a later decode failure, while the page watermark remains behind them"
    );
    canonical.execute(
        "UPDATE messages SET data=?1 WHERE message_id='three'",
        [serde_json::to_string(&messages[2])?],
    )?;
    let repaired = store.reindex_search(None, 2)?;
    assert_eq!(repaired.indexed_messages, 2);
    assert!(repaired.complete);
    for term in ["lapis", "sienna", "scarlet"] {
        assert_eq!(store.search_messages(term, None, 10)?.len(), 1);
    }
    assert_eq!(store.reindex_search(None, 2)?.indexed_messages, 0);
    Ok(())
}
