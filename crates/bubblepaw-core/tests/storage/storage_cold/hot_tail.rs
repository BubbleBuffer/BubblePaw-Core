use super::*;

#[test]
fn advancing_hot_tail_processes_only_newly_cold_rows_and_changed_refs_revisit_released_rows()
-> Result<()> {
    let dir = tempfile::tempdir()?;
    let compression_options = StoreOptions {
        cold_min_messages: u32::MAX,
        cold_min_bytes: 64 * 1024,
        ..options()
    };
    let store = Store::open_with_options(dir.path(), compression_options)?;
    let root =
        store.create_session_with_strategy("incremental cold pass", ContextStrategy::Classical)?;
    let messages: Vec<_> = (0..80).map(|index| message(&root.id, index)).collect();
    store.append_batch(&messages)?;
    store.save_context_checkpoint(&ContextCheckpoint {
        id: "incremental-summary".into(),
        session_id: root.id.clone(),
        through_message_id: messages[79].id.clone(),
        model: "synthetic".into(),
        summary: "Synthetic summary".into(),
        files_touched: vec![],
        created_at: "2026-10-12T00:00:00Z".into(),
        usage: None,
    })?;
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
    // Compression itself can make physical rows smaller than the initial size
    // gate; append-only canonical history still qualifies for later passes.
    assert!(
        (conn.query_row("SELECT SUM(length(data)) FROM messages", [], |row| row
            .get::<_, i64>(0))? as u64)
            < compression_options.cold_min_bytes
    );
    let old_body: String = conn.query_row(
        "SELECT data FROM messages WHERE message_id=?1",
        [&messages[3].id],
        |row| row.get(0),
    )?;
    conn.execute(
        "UPDATE messages SET data='already processed poison: invalid JSON' WHERE message_id=?1",
        [&messages[3].id],
    )?;
    for index in 80..83 {
        store.append_batch(&[message(&root.id, index)])?;
        let commits = store.storage_stats()?.chat_transaction_count;
        assert_eq!(store.compress_cold_messages(&root.id)?, 1);
        assert_eq!(commits + 1, store.storage_stats()?.chat_transaction_count);
        assert_eq!(store.compress_cold_messages(&root.id)?, 0);
        assert_eq!(commits + 1, store.storage_stats()?.chat_transaction_count);
    }
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM message_dictionaries", [], |row| row
            .get::<_, i64>(
            0
        ))?,
        1
    );
    conn.execute(
        "UPDATE messages SET data=?1 WHERE message_id=?2",
        params![old_body, messages[3].id],
    )?;
    let protected: String = conn.query_row(
        "SELECT data FROM messages WHERE message_id=?1",
        [&messages[0].id],
        |row| row.get(0),
    )?;
    assert!(!protected.contains("_row_codec"));
    view.prefix = vec![messages[1].clone()];
    store.save_context_view(&view)?;
    assert_eq!(store.compress_cold_messages(&root.id)?, 1);
    let released: String = conn.query_row(
        "SELECT data FROM messages WHERE message_id=?1",
        [&messages[0].id],
        |row| row.get(0),
    )?;
    assert!(released.contains("_row_codec"));
    assert_eq!(
        old_body,
        conn.query_row(
            "SELECT data FROM messages WHERE message_id=?1",
            [&messages[3].id],
            |row| row.get::<_, String>(0)
        )?
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM message_dictionaries", [], |row| row
            .get::<_, i64>(
            0
        ))?,
        1
    );
    Ok(())
}
