//! Synthetic cold-storage acceptance. No model or private transcript input.
use anyhow::Result;
use bubblepaw_core::{
    ContextCheckpoint, ContextStrategy, ContextView, Message, ModelRequest, Role, Store,
    StoreOptions, ToolCall,
};
use rusqlite::{Connection, params};
use serde_json::json;
use std::path::Path;

#[test]
fn dictionary_pins_survive_more_than_64_open_files_and_expire_with_the_reader() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_with_options(dir.path(), options())?;
    let mut hashes = std::collections::HashSet::new();
    let mut first = None;
    for file in 0..70 {
        let root = store.create_session_with_strategy(
            format!("pinned dictionary {file}"),
            ContextStrategy::Classical,
        )?;
        let messages: Vec<_> = (0..24)
            .map(|index| {
                let mut message = message(&root.id, index);
                message.content = format!("file-specific-pattern-{file}: {}", message.content);
                message
            })
            .collect();
        store.append_batch(&messages)?;
        store.save_context_checkpoint(&ContextCheckpoint {
            id: format!("pin-summary-{file}"),
            session_id: root.id.clone(),
            through_message_id: messages[23].id.clone(),
            model: "synthetic".into(),
            summary: "Synthetic summary".into(),
            files_touched: vec![],
            created_at: "2026-10-12T00:00:00Z".into(),
            usage: None,
        })?;
        assert!(store.compress_cold_messages(&root.id)? > 0);
        assert_eq!(store.messages(&root.id)?, messages);
        let path = store.chat_path(&root.id)?.unwrap();
        let conn = Connection::open(&path)?;
        let (hash, bytes): (String, Vec<u8>) =
            conn.query_row("SELECT hash,data FROM message_dictionaries", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
        hashes.insert(hash.clone());
        if file == 0 {
            let mut before = Vec::new();
            Store::export_data_dir_jsonl(dir.path(), &root.id, &mut before)?;
            // Corrupt its SQLite copy before opening another 69 dictionaries.
            conn.execute(
                "UPDATE message_dictionaries SET data=?1 WHERE hash=?2",
                params![b"corrupt synthetic dictionary".as_slice(), hash],
            )?;
            first = Some((root.id, path, hash, bytes, messages, before));
        }
    }
    assert!(hashes.len() > 64);
    let (session, path, hash, bytes, messages, before) = first.unwrap();
    // Open-file pins still supply the already validated dictionary, including
    // newly opened read-only iterators sharing its weak hash registry.
    assert_eq!(store.messages(&session)?, messages);
    let mut after = Vec::new();
    Store::export_data_dir_jsonl(dir.path(), &session, &mut after)?;
    assert_eq!(before, after);
    store.idle_checkpoint()?;
    drop(store);
    // Every reader has closed, so weak entries cannot hide a corrupt first load.
    assert!(Store::export_data_dir_jsonl(dir.path(), &session, Vec::new()).is_err());
    Connection::open(&path)?.execute(
        "UPDATE message_dictionaries SET data=?1 WHERE hash=?2",
        params![bytes, hash],
    )?;
    let reopened = Store::open_with_options(dir.path(), options())?;
    assert_eq!(reopened.messages(&session)?, messages);
    Ok(())
}

fn options() -> StoreOptions {
    StoreOptions {
        archive_after_days: 0,
        cold_min_messages: 16,
        cold_min_bytes: 1024,
        cold_tail_messages: 8,
        ..StoreOptions::default()
    }
}
fn message(session: &str, index: usize) -> Message {
    Message {
        id: format!("message-{index}"),
        session_id: session.into(),
        role: if index.is_multiple_of(2) {
            Role::User
        } else {
            Role::Assistant
        },
        origin: None,
        content: format!(
            "synthetic canonical sample {index}: {}",
            "A repeated sentence with unicode 猫 and spelling. ".repeat(24)
        ),
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: if !index.is_multiple_of(2) {
            Some(
                json!({"opaque":"signature spelling unchanged", "reasoning":"signed synthetic bytes", "number":-0.0}),
            )
        } else {
            None
        },
        is_error: false,
        created_at: "2026-10-12T00:00:00Z".into(),
    }
}
fn state(dir: &Path, id: &str) -> Result<String> {
    Ok(Connection::open(dir.join("catalog.db"))?.query_row(
        "SELECT storage_state FROM sessions WHERE id=?1",
        [id],
        |row| row.get(0),
    )?)
}
fn request(messages: Vec<Message>) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&ModelRequest {
        model: "synthetic".into(),
        system_prompt: "signed roundtrip".into(),
        messages,
        ..ModelRequest::default()
    })?)
}

fn check_history_pages(
    store: &Store,
    session: &str,
    messages: &[Message],
    limit: usize,
) -> Result<()> {
    let expected: Vec<_> = messages
        .iter()
        .map(|message| Ok((store.message_seq(session, &message.id)?, message.clone())))
        .collect::<Result<_>>()?;
    let mut restored = Vec::new();
    let mut before = None;
    let mut end = expected.len();
    loop {
        let mut page = store
            .messages_before(session, before.as_deref(), limit)?
            .expect("session-owned cursor");
        let start = end.saturating_sub(limit);
        assert_eq!(page, expected[start..end]);
        if page.is_empty() {
            break;
        }
        before = Some(page[0].1.id.clone());
        page.append(&mut restored);
        restored = page;
        end = start;
    }
    assert_eq!(restored, expected);
    Ok(())
}

#[test]
fn messages_before_orders_limits_and_scopes_message_id_cursors() -> Result<()> {
    let store = Store::in_memory()?;
    let root = store.create_session("paged root")?;
    let child =
        store.create_session_with_parent("paged child", ContextStrategy::Lcm, Some(&root.id))?;
    let empty = store.create_session("empty history")?;
    let messages: Vec<_> = (0..11).map(|index| message(&root.id, index)).collect();
    let mut interleaved = Vec::new();
    for (index, source) in messages.iter().enumerate() {
        // Shared IDs in another session must never choose its sequence cursor.
        interleaved.push(message(&child.id, index));
        interleaved.push(source.clone());
    }
    let foreign = message(&child.id, 1000);
    interleaved.push(foreign.clone());
    store.append_batch(&interleaved)?;
    let expected: Vec<_> = messages
        .iter()
        .map(|message| Ok((store.message_seq(&root.id, &message.id)?, message.clone())))
        .collect::<Result<_>>()?;
    assert_eq!(
        store.messages_before(&root.id, None, 3)?,
        Some(expected[8..].to_vec())
    );
    assert_eq!(
        store.messages_before(&root.id, Some(&messages[8].id), 3)?,
        Some(expected[5..8].to_vec())
    );
    assert_eq!(store.messages_before(&root.id, None, 100)?, Some(expected));
    assert_eq!(store.messages_before(&root.id, None, 0)?, Some(vec![]));
    assert_eq!(
        store.messages_before(&root.id, Some(&messages[8].id), 0)?,
        Some(vec![])
    );
    assert_eq!(
        store.messages_before(&root.id, Some(&messages[0].id), 3)?,
        Some(vec![])
    );
    assert_eq!(
        store.messages_before(&root.id, Some("unknown-message"), 3)?,
        None
    );
    assert_eq!(store.messages_before(&root.id, Some(&foreign.id), 3)?, None);
    assert_eq!(store.messages_before(&empty.id, None, 3)?, Some(vec![]));
    assert_eq!(
        store.messages_before(&empty.id, Some(&messages[0].id), 3)?,
        None
    );
    check_history_pages(&store, &root.id, &messages, 3)?;
    Ok(())
}

#[test]
fn messages_before_restores_archived_dictionary_rows_and_preserves_exact_history() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_with_options(dir.path(), options())?;
    let root = store.create_session_with_strategy("paged cold root", ContextStrategy::Classical)?;
    let child = store.create_session_with_parent(
        "paged cold child",
        ContextStrategy::Lcm,
        Some(&root.id),
    )?;
    let messages: Vec<_> = (0..80).map(|index| message(&root.id, index)).collect();
    let child_messages: Vec<_> = (0..12).map(|index| message(&child.id, index)).collect();
    store.append_batch(&messages)?;
    store.append_batch(&child_messages)?;
    store.save_context_checkpoint(&ContextCheckpoint {
        id: "paged-summary".into(),
        session_id: root.id.clone(),
        through_message_id: messages[69].id.clone(),
        model: "synthetic".into(),
        summary: "Synthetic summary".into(),
        files_touched: vec![],
        created_at: "2026-10-12T00:00:00Z".into(),
        usage: None,
    })?;
    assert!(store.compress_cold_messages(&root.id)? > 40);
    let path = store.chat_path(&root.id)?.unwrap();
    let conn = Connection::open(&path)?;
    let body: String = conn.query_row(
        "SELECT data FROM messages WHERE session_id=?1 AND message_id=?2",
        params![root.id, messages[3].id],
        |row| row.get(0),
    )?;
    assert!(body.contains("_row_codec"));
    let cursor_plan: String = conn.query_row(
        "EXPLAIN QUERY PLAN SELECT seq FROM messages WHERE session_id=?1 AND message_id=?2",
        params![root.id, messages[3].id],
        |row| row.get(3),
    )?;
    assert!(cursor_plan.contains("messages_id"), "{cursor_plan}");
    drop(conn);
    assert!(store.archive_chat(&root.id)?);
    assert!(!path.exists());
    assert!(path.with_extension("db.zst").exists());
    store.idle_checkpoint()?;
    drop(store);
    let reopened = Store::open_with_options(dir.path(), options())?;
    assert_eq!(state(dir.path(), &root.id)?, "archived");
    assert!(!path.exists());
    // The first cursor read restores the root file through the scoped path.
    let page = reopened
        .messages_before(&child.id, Some(&child_messages[8].id), 3)?
        .expect("archived child cursor");
    assert_eq!(
        page.into_iter()
            .map(|(_, message)| message)
            .collect::<Vec<_>>(),
        child_messages[5..8]
    );
    assert!(path.exists());
    assert!(!path.with_extension("db.zst").exists());
    assert_eq!(state(dir.path(), &root.id)?, "active");
    assert_eq!(state(dir.path(), &child.id)?, "active");
    check_history_pages(&reopened, &root.id, &messages, 13)?;
    check_history_pages(&reopened, &child.id, &child_messages, 5)?;
    assert_eq!(
        reopened.messages_before(&child.id, Some(&messages[79].id), 3)?,
        None
    );
    Ok(())
}

#[test]
fn messages_before_decodes_only_selected_rows_and_never_the_cursor_body() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_with_options(dir.path(), options())?;
    let root = store.create_session("bounded paged decode")?;
    let messages: Vec<_> = (0..12).map(|index| message(&root.id, index)).collect();
    store.append_batch(&messages)?;
    let path = store.chat_path(&root.id)?.unwrap();
    let conn = Connection::open(path)?;
    for index in [0, 8, 11] {
        conn.execute(
            "UPDATE messages SET data='poisoned out-of-page body: invalid JSON' WHERE message_id=?1",
            [&messages[index].id],
        )?;
    }
    let page = store
        .messages_before(&root.id, Some(&messages[8].id), 3)?
        .expect("cursor metadata survives poisoned body");
    assert_eq!(
        page.into_iter()
            .map(|(_, message)| message)
            .collect::<Vec<_>>(),
        messages[5..8]
    );
    let page = store
        .messages_before(&root.id, Some(&messages[11].id), 2)?
        .expect("newest cursor metadata survives poisoned body");
    assert_eq!(
        page.into_iter()
            .map(|(_, message)| message)
            .collect::<Vec<_>>(),
        messages[9..11]
    );
    assert_eq!(
        store.messages_before(&root.id, Some(&messages[0].id), 3)?,
        Some(vec![])
    );
    assert_eq!(store.messages_before(&root.id, None, 0)?, Some(vec![]));
    assert_eq!(
        store.messages_before(&root.id, Some("unknown-message"), 3)?,
        None
    );
    // Selected corruption still propagates through the ordinary decoder.
    assert!(store.messages_before(&root.id, None, 1).is_err());
    assert!(
        store
            .messages_before(&root.id, Some(&messages[8].id), 100)
            .is_err()
    );
    Ok(())
}

#[test]
fn dictionary_rows_preserve_requests_view_references_and_hot_tail() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_with_options(dir.path(), options())?;
    let session = store.create_session_with_strategy("cold rows", ContextStrategy::Classical)?;
    let messages: Vec<_> = (0..80).map(|index| message(&session.id, index)).collect();
    store.append_batch(&messages)?;
    let checkpoint = ContextCheckpoint {
        id: "summary".into(),
        session_id: session.id.clone(),
        through_message_id: messages[69].id.clone(),
        model: "synthetic".into(),
        summary: "Synthetic summary".into(),
        files_touched: vec![],
        created_at: "2026-10-12T00:00:00Z".into(),
        usage: None,
    };
    store.save_context_checkpoint(&checkpoint)?;
    let frontier = store.message_seq(&session.id, &messages[69].id)?;
    let mut summary = message(&session.id, 100);
    summary.role = Role::System;
    summary.content = checkpoint.summary.clone();
    let view = ContextView {
        session_id: session.id.clone(),
        frontier: Some(frontier),
        system_prompt: "frozen".into(),
        prefix: vec![summary, messages[2].clone()],
        masked_tool_output_ids: vec![],
        clipped_tool_call_ids: vec![],
        created_at: "2026-10-12T00:00:00Z".into(),
        threshold: 100,
        budget: 100,
        tokens: 80,
        blocks: Vec::new(),
        blocks_estimated: true,
    };
    store.save_context_view(&view)?;
    let before = request(store.messages(&session.id)?)?;
    let before_view = serde_json::to_vec(&store.context_view(&session.id)?)?;
    assert!(store.compress_cold_messages(&session.id)? > 40);
    assert_eq!(before, request(store.messages(&session.id)?)?);
    assert_eq!(
        before_view,
        serde_json::to_vec(&store.context_view(&session.id)?)?
    );
    let path = store.chat_path(&session.id)?.unwrap();
    let conn = Connection::open(path)?;
    let dictionaries: i64 =
        conn.query_row("SELECT COUNT(*) FROM message_dictionaries", [], |row| {
            row.get(0)
        })?;
    assert_eq!(dictionaries, 1);
    for index in [2, 70, 79] {
        let body: String = conn.query_row(
            "SELECT data FROM messages WHERE message_id=?1",
            [&messages[index].id],
            |row| row.get(0),
        )?;
        assert!(!body.contains("_row_codec"));
    }
    drop(conn);
    store.idle_checkpoint()?;
    drop(store);
    let reopened = Store::open_with_options(dir.path(), options())?;
    assert_eq!(before, request(reopened.messages(&session.id)?)?);
    let mut seen = 0;
    assert_eq!(
        reopened.for_each_message_range_while(&session.id, i64::MIN, i64::MAX, |_, _| {
            seen += 1;
            Ok(seen < 3)
        })?,
        3
    );
    let mut ids = Vec::new();
    reopened.for_each_message_index(&session.id, |seq, id| {
        ids.push((seq, id.to_owned()));
        Ok(())
    })?;
    assert_eq!(ids.len(), 80);
    assert_eq!(
        reopened
            .messages_range(&session.id, ids[9].0, ids[19].0, 3)?
            .len(),
        3
    );
    assert_eq!(reopened.message_bounds(&session.id)?.2, 80);
    reopened.idle_checkpoint()?;
    Ok(())
}

#[test]
fn whole_chat_archive_exports_rebuilds_and_restores_tree() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_with_options(dir.path(), options())?;
    let root = store.create_session("root")?;
    let child = store.create_session_with_parent("child", ContextStrategy::Lcm, Some(&root.id))?;
    store.append_batch(&[message(&root.id, 0), message(&child.id, 1)])?;
    let path = store.chat_path(&root.id)?.unwrap();
    let mut before = Vec::new();
    store.export_jsonl(&root.id, &mut before)?;
    assert!(store.archive_chat(&root.id)?);
    assert!(!path.exists());
    assert!(path.with_extension("db.zst").exists());
    assert_eq!(state(dir.path(), &root.id)?, "archived");
    assert_eq!(state(dir.path(), &child.id)?, "archived");
    let mut exported = Vec::new();
    Store::export_data_dir_jsonl(dir.path(), &root.id, &mut exported)?;
    assert_eq!(before, exported);
    assert!(!path.exists());
    assert_eq!(store.catalog_rebuild()?, 2);
    assert!(!path.exists());
    let stats = store.storage_stats()?;
    assert_eq!(stats.message_count, 2);
    assert!(stats.chat_bytes > 0);
    assert!(!path.exists());
    store.idle_checkpoint()?;
    drop(store);
    std::fs::remove_file(dir.path().join("catalog.db"))?;
    let reopened = Store::open_with_options(dir.path(), options())?;
    assert_eq!(state(dir.path(), &root.id)?, "archived");
    assert!(!path.exists());
    assert_eq!(reopened.messages(&child.id)?, vec![message(&child.id, 1)]);
    assert!(path.exists());
    assert!(!path.with_extension("db.zst").exists());
    assert_eq!(state(dir.path(), &root.id)?, "active");
    Ok(())
}

#[test]
fn interrupted_archive_steps_preserve_original_or_verified_archive() -> Result<()> {
    for step in 0..4 {
        let dir = tempfile::tempdir()?;
        let store = Store::open_with_options(dir.path(), options())?;
        let root = store.create_session("crash fixture")?;
        store.append_batch(&[message(&root.id, 0)])?;
        let path = store.chat_path(&root.id)?.unwrap();
        store.idle_checkpoint()?;
        drop(store);
        let archive = path.with_extension("db.zst");
        if step >= 1 {
            let bytes = std::fs::read(&path)?;
            std::fs::write(&archive, zstd::stream::encode_all(bytes.as_slice(), 3)?)?;
        } else {
            std::fs::write(
                format!("{}.part", archive.display()),
                b"interrupted partial zstd",
            )?;
        }
        if step == 2 {
            std::fs::remove_file(&path)?;
        }
        if step == 3 {
            std::fs::write(&archive, b"corrupt archive")?;
        }
        Connection::open(dir.path().join("catalog.db"))?.execute(
            "UPDATE sessions SET storage_state='archiving' WHERE root_id=?1",
            [&root.id],
        )?;
        let reopened = Store::open_with_options(dir.path(), options())?;
        assert_eq!(reopened.messages(&root.id)?, vec![message(&root.id, 0)]);
    }
    Ok(())
}

#[test]
fn archive_age_is_tree_wide_and_zero_disables() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_with_options(dir.path(), options())?;
    let root = store.create_session("old")?;
    let path = store.chat_path(&root.id)?.unwrap();
    store.idle_checkpoint()?;
    drop(store);
    let conn = Connection::open(&path)?;
    let data: String =
        conn.query_row("SELECT data FROM sessions WHERE id=?1", [&root.id], |row| {
            row.get(0)
        })?;
    let mut data: serde_json::Value = serde_json::from_str(&data)?;
    data["created_at"] = "2000-01-01T00:00:00Z".into();
    data["updated_at"] = "2000-01-01T00:00:00Z".into();
    conn.execute(
        "UPDATE sessions SET data=?1 WHERE id=?2",
        params![data.to_string(), root.id],
    )?;
    drop(conn);
    let disabled = Store::open_with_options(dir.path(), options())?;
    assert!(path.exists());
    assert_eq!(disabled.archive_idle_chats()?, 0);
    disabled.idle_checkpoint()?;
    drop(disabled);
    let enabled = Store::open_with_options(
        dir.path(),
        StoreOptions {
            archive_after_days: 30,
            ..options()
        },
    )?;
    assert!(!path.exists());
    assert_eq!(state(dir.path(), &root.id)?, "archived");
    drop(enabled);
    let tree = tempfile::tempdir()?;
    let store = Store::open_with_options(
        tree.path(),
        StoreOptions {
            archive_after_days: 30,
            ..options()
        },
    )?;
    let root = store.create_session("old root with recent child")?;
    let child =
        store.create_session_with_parent("recent child", ContextStrategy::Lcm, Some(&root.id))?;
    store.flush_serving()?;
    Connection::open(tree.path().join("catalog.db"))?.execute(
        "UPDATE sessions SET last_activity='2000-01-01T00:00:00Z' WHERE id=?1",
        [&root.id],
    )?;
    assert_eq!(store.archive_idle_chats()?, 0);
    assert_eq!(state(tree.path(), &root.id)?, "active");
    assert_eq!(state(tree.path(), &child.id)?, "active");
    Ok(())
}

#[test]
fn installed_restore_crash_and_source_compaction_boundary_roundtrip() -> Result<()> {
    use bubblepaw_core::transcript::ImportDerivedRecord;
    let dir = tempfile::tempdir()?;
    let store = Store::open_with_options(dir.path(), options())?;
    let root = store.create_session("imported cold")?;
    let messages: Vec<_> = (0..80).map(|index| message(&root.id, index)).collect();
    let derived = |id: &str, kind: &str, index: usize| ImportDerivedRecord {
        id: id.into(),
        session_id: root.id.clone(),
        source_line: index as u64,
        source_kind: kind.into(),
        through_message_id: Some(messages[index].id.clone()),
        content: json!({"message":"source summary"}),
        created_at: "2026-10-12T00:00:00Z".into(),
        origin: "import".into(),
    };
    store.append_import_batch(
        &messages,
        &[
            derived("compaction", "codex_compaction", 40),
            derived("reasoning", "codex_reasoning", 79),
        ],
    )?;
    store.set_import_metadata(&root.id, &json!({"source":"synthetic-fixture"}))?;
    assert!(store.compress_cold_messages(&root.id)? > 16);
    let path = store.chat_path(&root.id)?.unwrap();
    let body: String = Connection::open(&path)?.query_row(
        "SELECT data FROM messages WHERE message_id=?1",
        [&messages[60].id],
        |row| row.get(0),
    )?;
    assert!(!body.contains("_row_codec"));
    let mut before = Vec::new();
    store.export_jsonl(&root.id, &mut before)?;
    assert!(store.archive_chat(&root.id)?);
    let mut offline = Vec::new();
    Store::export_data_dir_jsonl(dir.path(), &root.id, &mut offline)?;
    assert_eq!(before, offline);
    store.idle_checkpoint()?;
    drop(store);
    let archive = path.with_extension("db.zst");
    std::fs::write(
        &path,
        zstd::stream::decode_all(std::fs::read(&archive)?.as_slice())?,
    )?;
    // Catalog still says archived: interruption happened after installed restore.
    let reopened = Store::open_with_options(dir.path(), options())?;
    assert_eq!(state(dir.path(), &root.id)?, "active");
    assert!(!archive.exists());
    assert_eq!(reopened.messages(&root.id)?, messages);
    assert_eq!(reopened.import_derived(&root.id)?.len(), 2);
    Ok(())
}

#[test]
fn phase_b_split_records_upgrade_without_rewriting_message_bodies() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_with_options(dir.path(), options())?;
    let root = store.create_session("phase B upgrade")?;
    let mut source = message(&root.id, 1);
    source.tool_calls.push(ToolCall {
        id: "historical-call".into(),
        name: "synthetic-tool".into(),
        arguments: json!({"path":"synthetic.txt","signed_zero":-0.0}),
    });
    store.append_batch(&[source])?;
    let before = request(store.messages(&root.id)?)?;
    let path = store.chat_path(&root.id)?.unwrap();
    store.idle_checkpoint()?;
    drop(store);
    let conn = Connection::open(&path)?;
    let body: String = conn.query_row("SELECT data FROM messages", [], |row| row.get(0))?;
    conn.execute_batch("DROP TABLE message_tool_calls; DROP TABLE message_headers; DROP TABLE message_dictionaries; PRAGMA user_version=9; PRAGMA wal_checkpoint(TRUNCATE);")?;
    drop(conn);
    let reopened = Store::open_with_options(dir.path(), options())?;
    assert_eq!(before, request(reopened.messages(&root.id)?)?);
    let conn = Connection::open(&path)?;
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?,
        12
    );
    assert_eq!(
        body,
        conn.query_row("SELECT data FROM messages", [], |row| row
            .get::<_, String>(0))?
    );
    assert_eq!(
        conn.query_row("SELECT call_id FROM message_tool_calls", [], |row| row
            .get::<_, String>(
            0
        ))?,
        "historical-call"
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM message_dictionaries", [], |row| row
            .get::<_, i64>(
            0
        ))?,
        0
    );
    assert_eq!(
        conn.query_row("SELECT role FROM message_headers", [], |row| row
            .get::<_, String>(0))?,
        "assistant"
    );
    Ok(())
}

#[test]
fn buffered_catalog_survives_serving_wipe_and_archived_deletion_stays_scoped() -> Result<()> {
    use bubblepaw_core::transcript::ImportDerivedRecord;
    let dir = tempfile::tempdir()?;
    let store = Store::open_with_options(dir.path(), options())?;
    let root = store.create_session("root")?;
    let child = store.create_session_with_parent("child", ContextStrategy::Lcm, Some(&root.id))?;
    let other = store.create_session("unrelated")?;
    let root_path = store.chat_path(&root.id)?.unwrap();
    let other_path = store.chat_path(&other.id)?.unwrap();
    let derived = ImportDerivedRecord {
        id: "child-source".into(),
        session_id: child.id.clone(),
        source_line: 1,
        source_kind: "codex_reasoning".into(),
        through_message_id: Some("message-0".into()),
        content: json!({"reasoning":"synthetic"}),
        created_at: "2026-10-12T00:00:00Z".into(),
        origin: "import".into(),
    };
    store.append_import_batch(&[message(&child.id, 0)], &[derived])?;
    store.set_import_metadata(&child.id, &json!({"source":"synthetic-child"}))?;
    store.append_batch(&[message(&other.id, 2)])?;
    // A record append leaves both serving terms and catalog activity buffered.
    store.serving_wipe()?;
    let new_child = store.create_session_with_parent(
        "post-wipe child",
        ContextStrategy::Lcm,
        Some(&root.id),
    )?;
    store.append_batch(&[message(&new_child.id, 3)])?;
    store.flush_serving()?;
    assert_eq!(state(dir.path(), &new_child.id)?, "active");
    assert!(store.archive_chat(&other.id)?);
    assert!(store.archive_chat(&root.id)?);
    assert!(
        store
            .search_messages("notpresentanywhere", None, 10)?
            .is_empty()
    );
    assert!(!root_path.exists());
    assert!(!other_path.exists());
    store.delete_session(&child.id)?;
    assert!(root_path.exists());
    assert!(!other_path.exists());
    assert_eq!(
        store.messages(&new_child.id)?,
        vec![message(&new_child.id, 3)]
    );
    let conn = Connection::open(&root_path)?;
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM imported_derived WHERE session_id=?1",
            [&child.id],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    drop(conn);
    store.delete_session(&root.id)?;
    assert!(!root_path.exists());
    assert!(!other_path.exists());
    assert_eq!(state(dir.path(), &other.id)?, "archived");
    store.delete_session(&other.id)?;
    assert!(!other_path.exists());
    assert!(!other_path.with_extension("db.zst").exists());
    Ok(())
}

#[test]
fn completed_cold_pass_skips_covered_rows_and_invalidates_only_on_eligibility_changes() -> Result<()>
{
    let dir = tempfile::tempdir()?;
    let store = Store::open_with_options(dir.path(), options())?;
    let root = store.create_session_with_strategy("pass watermark", ContextStrategy::Classical)?;
    let messages: Vec<_> = (0..80).map(|index| message(&root.id, index)).collect();
    store.append_batch(&messages)?;
    let mut checkpoint = ContextCheckpoint {
        id: "first-pass-summary".into(),
        session_id: root.id.clone(),
        through_message_id: messages[79].id.clone(),
        model: "synthetic".into(),
        summary: "Synthetic summary".into(),
        files_touched: vec![],
        created_at: "2026-10-12T00:00:00Z".into(),
        usage: None,
    };
    store.save_context_checkpoint(&checkpoint)?;
    let mut view = ContextView {
        session_id: root.id.clone(),
        frontier: Some(store.message_seq(&root.id, &messages[79].id)?),
        system_prompt: "frozen".into(),
        prefix: vec![messages[0].clone()],
        masked_tool_output_ids: vec![],
        clipped_tool_call_ids: vec![],
        created_at: "2026-10-12T00:00:00Z".into(),
        threshold: 100,
        budget: 100,
        tokens: 80,
        blocks: Vec::new(),
        blocks_estimated: true,
    };
    store.save_context_view(&view)?;
    assert!(store.compress_cold_messages(&root.id)? > 40);
    let path = store.chat_path(&root.id)?.unwrap();
    let conn = Connection::open(&path)?;
    let encoded: String = conn.query_row(
        "SELECT data FROM messages WHERE message_id=?1",
        [&messages[3].id],
        |row| row.get(0),
    )?;
    assert!(encoded.contains("_row_codec"));
    let commits = store.storage_stats()?.chat_transaction_count;
    conn.execute(
        "UPDATE messages SET data='covered poison: invalid JSON' WHERE message_id=?1",
        [&messages[3].id],
    )?;
    assert_eq!(store.compress_cold_messages(&root.id)?, 0);
    store.idle_checkpoint()?;
    assert_eq!(commits, store.storage_stats()?.chat_transaction_count);
    conn.execute(
        "UPDATE messages SET data=?1 WHERE message_id=?2",
        params![encoded, messages[3].id],
    )?;
    assert_eq!(
        encoded,
        conn.query_row(
            "SELECT data FROM messages WHERE message_id=?1",
            [&messages[3].id],
            |row| row.get::<_, String>(0)
        )?
    );
    drop(conn);
    // A different protected prefix invalidates the receipt and releases row 0.
    view.prefix = vec![messages[1].clone()];
    store.save_context_view(&view)?;
    let commits = store.storage_stats()?.chat_transaction_count;
    assert!(store.compress_cold_messages(&root.id)? > 0);
    assert_eq!(commits + 1, store.storage_stats()?.chat_transaction_count);
    let changed = StoreOptions {
        cold_row_min_bytes: 257,
        ..options()
    };
    let commits = store.storage_stats()?.chat_transaction_count;
    assert_eq!(
        store.compress_cold_messages_with_options(&root.id, changed)?,
        0
    );
    assert_eq!(commits + 1, store.storage_stats()?.chat_transaction_count);
    assert_eq!(
        store.compress_cold_messages_with_options(&root.id, changed)?,
        0
    );
    assert_eq!(commits + 1, store.storage_stats()?.chat_transaction_count);
    checkpoint.id = "replacement-summary-same-boundary".into();
    checkpoint.summary = "Replacement synthetic summary".into();
    store.save_context_checkpoint(&checkpoint)?;
    let commits = store.storage_stats()?.chat_transaction_count;
    store.compress_cold_messages_with_options(&root.id, changed)?;
    assert_eq!(commits + 1, store.storage_stats()?.chat_transaction_count);
    store.append_batch(&[message(&root.id, 80)])?;
    let commits = store.storage_stats()?.chat_transaction_count;
    assert!(store.compress_cold_messages_with_options(&root.id, changed)? > 0);
    assert_eq!(commits + 1, store.storage_stats()?.chat_transaction_count);
    assert_eq!(
        store.compress_cold_messages_with_options(&root.id, changed)?,
        0
    );
    assert_eq!(commits + 1, store.storage_stats()?.chat_transaction_count);
    Ok(())
}

#[test]
fn insufficient_dictionary_training_is_cached_and_short_histories_keep_the_entire_hot_tail()
-> Result<()> {
    let store = Store::in_memory()?;
    let root = store.create_session("short cold training")?;
    let messages: Vec<_> = (0..7).map(|index| message(&root.id, index)).collect();
    store.append_batch(&messages)?;
    store.save_context_checkpoint(&ContextCheckpoint {
        id: "short-summary".into(),
        session_id: root.id.clone(),
        through_message_id: messages[6].id.clone(),
        model: "synthetic".into(),
        summary: "Synthetic summary".into(),
        files_touched: vec![],
        created_at: "2026-10-12T00:00:00Z".into(),
        usage: None,
    })?;
    let tiny = StoreOptions {
        cold_tail_messages: 0,
        cold_min_messages: 0,
        cold_min_bytes: 0,
        ..options()
    };
    let commits = store.storage_stats()?.chat_transaction_count;
    assert_eq!(
        store.compress_cold_messages_with_options(&root.id, tiny)?,
        0
    );
    let after = store.storage_stats()?.chat_transaction_count;
    assert_eq!(commits + 1, after);
    assert_eq!(
        store.compress_cold_messages_with_options(&root.id, tiny)?,
        0
    );
    assert_eq!(after, store.storage_stats()?.chat_transaction_count);
    assert_eq!(
        store.compress_cold_messages_with_options(
            &root.id,
            StoreOptions {
                cold_tail_messages: 128,
                ..tiny
            }
        )?,
        0
    );
    assert_eq!(after, store.storage_stats()?.chat_transaction_count);
    assert_eq!(store.messages(&root.id)?, messages);
    Ok(())
}

#[test]
fn known_archives_stay_packed_at_startup_and_explicit_rebuild_repairs_missing_children()
-> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_with_options(dir.path(), options())?;
    let root = store.create_session("known archived root")?;
    let child = store.create_session_with_parent(
        "known archived child",
        ContextStrategy::Lcm,
        Some(&root.id),
    )?;
    let unrelated = store.create_session("unrelated active root")?;
    store.append_batch(&[message(&root.id, 0), message(&child.id, 1)])?;
    let path = store.chat_path(&root.id)?.unwrap();
    assert!(store.archive_chat(&root.id)?);
    store.idle_checkpoint()?;
    drop(store);
    let archive = path.with_extension("db.zst");
    let packed = std::fs::read(&archive)?;
    std::fs::write(&archive, b"synthetic poisoned archive: invalid zstd")?;
    let reopened = Store::open_with_options(dir.path(), options())?;
    assert_eq!(reopened.list_sessions()?.len(), 3);
    assert!(
        reopened
            .search_messages("definitelyunmatchedtoken", None, 10)?
            .is_empty()
    );
    assert!(!path.exists());
    reopened.delete_session(&unrelated.id)?;
    assert_eq!(reopened.list_sessions()?.len(), 2);
    assert!(!path.exists());
    assert!(reopened.messages(&child.id).is_err());
    assert!(!path.exists());
    assert!(reopened.catalog_rebuild().is_err());
    assert_eq!(state(dir.path(), &root.id)?, "archived");
    std::fs::write(&archive, &packed)?;
    assert_eq!(reopened.messages(&child.id)?, vec![message(&child.id, 1)]);
    assert!(reopened.archive_chat(&root.id)?);
    reopened.idle_checkpoint()?;
    drop(reopened);
    Connection::open(dir.path().join("catalog.db"))?
        .execute("DELETE FROM sessions WHERE id=?1", [&child.id])?;
    let partial = Store::open_with_options(dir.path(), options())?;
    assert_eq!(partial.list_sessions()?.len(), 1);
    assert!(!path.exists());
    assert_eq!(partial.catalog_rebuild()?, 2);
    assert_eq!(partial.list_sessions()?.len(), 2);
    assert!(!path.exists());
    partial.idle_checkpoint()?;
    drop(partial);
    Connection::open(dir.path().join("catalog.db"))?.execute("DELETE FROM sessions", [])?;
    let uncatalogued = Store::open_with_options(dir.path(), options())?;
    assert_eq!(uncatalogued.list_sessions()?.len(), 2);
    assert_eq!(state(dir.path(), &root.id)?, "archived");
    assert!(!path.exists());
    Ok(())
}

#[path = "storage_cold/hot_tail.rs"]
mod hot_tail;
