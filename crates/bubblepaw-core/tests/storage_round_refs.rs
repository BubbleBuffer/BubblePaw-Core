//! Boundary validation uses references while canonical bodies remain lazy.
use anyhow::Result;
use bubblepaw_core::{
    ContextCheckpoint, ContextStrategy, Message, Role, Store, StoreOptions, ToolCall,
};
use serde_json::json;

fn message(session: &str, index: usize, role: Role) -> Message {
    Message {
        id: format!("round-{index}"),
        session_id: session.into(),
        role,
        origin: None,
        content: format!(
            "Synthetic message {index}: {}",
            "Repeated exact text 猫\n".repeat(48)
        ),
        tool_calls: Vec::new(),
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: "2026-10-12T00:00:00Z".into(),
    }
}
fn call() -> ToolCall {
    ToolCall {
        id: "source-reused-id".into(),
        name: "synthetic".into(),
        arguments: json!({}),
    }
}

#[test]
fn source_reused_call_ids_preserve_each_exact_prefix_pending_state() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("source boundaries")?;
    let mut messages = Vec::new();
    for index in 0..4 {
        let mut message = message(
            &session.id,
            index,
            if index.is_multiple_of(2) {
                Role::Assistant
            } else {
                Role::Tool
            },
        );
        if index.is_multiple_of(2) {
            message.tool_calls.push(call());
        } else {
            message.tool_call_id = Some(call().id);
        }
        messages.push(message);
    }
    store.append_batch(&messages)?;
    for (index, message) in messages.iter().enumerate() {
        let pending = store
            .pending_call_ids_through(&session.id, store.message_seq(&session.id, &message.id)?)?;
        assert_eq!(
            pending,
            if index.is_multiple_of(2) {
                vec![call().id]
            } else {
                Vec::new()
            }
        );
    }
    Ok(())
}

#[test]
fn advancing_complete_classical_checkpoint_does_not_decode_covered_bodies() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_with_options(
        directory.path(),
        StoreOptions {
            archive_after_days: 0,
            cold_min_messages: 16,
            cold_min_bytes: 1024,
            cold_tail_messages: 8,
            ..Default::default()
        },
    )?;
    let session =
        store.create_session_with_strategy("cold boundary", ContextStrategy::Classical)?;
    let mut messages: Vec<_> = (0..80)
        .map(|index| message(&session.id, index, Role::Assistant))
        .collect();
    messages[2].tool_calls.push(call());
    messages[3].role = Role::Tool;
    messages[3].tool_call_id = Some(call().id);
    store.append_batch(&messages)?;
    let checkpoint = |id: &str, through: &Message| ContextCheckpoint {
        id: id.into(),
        session_id: session.id.clone(),
        through_message_id: through.id.clone(),
        model: "synthetic".into(),
        summary: "Already retained summary.".into(),
        files_touched: Vec::new(),
        created_at: "2026-10-12T00:00:00Z".into(),
        usage: None,
    };
    store.save_context_checkpoint(&checkpoint("first", &messages[69]))?;
    assert!(store.compress_cold_messages(&session.id)? > 40);
    let connection = rusqlite::Connection::open(store.chat_path(&session.id)?.unwrap())?;
    connection.execute(
        "UPDATE messages SET data='unreadable covered body' WHERE message_id=?1",
        [&messages[0].id],
    )?;
    drop(connection);
    assert!(store.messages(&session.id).is_err());
    let mut headers = Vec::new();
    store.for_each_message_header(&session.id, |_, message| {
        assert!(message.content.is_empty());
        assert!(message.provider_state.is_none());
        assert!(
            message
                .tool_calls
                .iter()
                .all(|call| call.name.is_empty() && call.arguments.is_null())
        );
        headers.push(message);
        Ok(())
    })?;
    assert_eq!(headers.len(), 80);
    assert_eq!(headers[2].tool_calls[0].id, call().id);
    assert_eq!(headers[3].tool_call_id.as_deref(), Some(call().id.as_str()));
    store.save_context_checkpoint(&checkpoint("next", &messages[79]))?;
    assert_eq!(store.context_checkpoint(&session.id)?.unwrap().id, "next");
    Ok(())
}

#[test]
fn headers_preserve_typed_classification_without_original_body_reads() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("header provenance")?;
    let mut typed = message(&session.id, 0, Role::User);
    typed.content = "A real user request".into();
    let mut generated = message(&session.id, 1, Role::User);
    generated.content = "Task:\nSynthetic child task".into();
    store.append_batch(&[typed, generated])?;
    let mut flags = Vec::new();
    store.for_each_message_header(&session.id, |_, message| {
        assert!(message.content.is_empty());
        flags.push(bubblepaw_core::is_typed_user(&message));
        Ok(())
    })?;
    assert_eq!(flags, [true, false]);
    Ok(())
}

#[test]
fn imported_summary_seeds_coverage_without_inventing_outcomes_or_copying_summary_text() -> Result<()>
{
    use bubblepaw_core::{ContextView, transcript::ImportDerivedRecord};
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let session =
        store.create_session_with_strategy("source coverage", ContextStrategy::Classical)?;
    let mut history = [
        message(&session.id, 0, Role::User),
        message(&session.id, 1, Role::Assistant),
        message(&session.id, 2, Role::User),
        message(&session.id, 3, Role::Assistant),
        message(&session.id, 4, Role::Tool),
    ];
    history[1].tool_calls.push(call());
    history[3].tool_calls.push(ToolCall {
        id: "new-call".into(),
        name: "synthetic".into(),
        arguments: json!({}),
    });
    history[4].tool_call_id = Some("new-call".into());
    let derived = ImportDerivedRecord {
        id: "source-summary".into(),
        session_id: session.id.clone(),
        source_line: 10,
        source_kind: "codex_compaction".into(),
        through_message_id: Some(history[1].id.clone()),
        content: json!({"message":"The source's own retained summary with Unicode 猫."}),
        created_at: history[1].created_at.clone(),
        origin: "import".into(),
    };
    store.append_import_batch(&history[..2], &[derived])?;
    store.append_batch(&history[2..])?;
    let empty = ImportDerivedRecord {
        id: "empty-source-summary".into(),
        source_line: 50,
        through_message_id: Some(history[4].id.clone()),
        content: json!({"message":""}),
        ..store.import_derived(&session.id)?.remove(0)
    };
    store.append_import_batch(&[], &[empty])?;
    let (frontier, prefix) = store.imported_history_seed(&session.id)?;
    assert_eq!(
        frontier,
        Some(store.message_seq(&session.id, &history[1].id)?)
    );
    assert_eq!(
        store.episode_coverage_token(&session.id)?,
        Some(history[1].id.clone())
    );
    let view = ContextView {
        session_id: session.id.clone(),
        frontier,
        system_prompt: "source serving".into(),
        prefix,
        masked_tool_output_ids: vec![],
        clipped_tool_call_ids: vec![],
        created_at: history[1].created_at.clone(),
        threshold: 10000,
        budget: 5000,
        tokens: 100,
    };
    store.save_context_view(&view)?;
    store.flush_serving()?;
    let serving = rusqlite::Connection::open(directory.path().join("serving.db"))?;
    let encoded: String =
        serving.query_row("SELECT data FROM context_views", [], |row| row.get(0))?;
    assert!(!encoded.contains(&view.prefix[0].content));
    assert!(encoded.contains("imported_derived"));
    drop(serving);
    let connection = rusqlite::Connection::open(store.chat_path(&session.id)?.unwrap())?;
    connection.execute(
        "UPDATE messages SET data='unreadable original source call' WHERE message_id=?1",
        [&history[1].id],
    )?;
    drop(connection);
    assert_eq!(
        serde_json::to_vec(&store.context_view(&session.id)?.unwrap())?,
        serde_json::to_vec(&view)?
    );
    let checkpoint = |through: usize| ContextCheckpoint {
        id: format!("native-{through}"),
        session_id: session.id.clone(),
        through_message_id: history[through].id.clone(),
        model: "synthetic".into(),
        summary: "A later native summary.".into(),
        files_touched: vec![],
        created_at: history[through].created_at.clone(),
        usage: None,
    };
    assert!(store.save_context_checkpoint(&checkpoint(3)).is_err());
    store.save_context_checkpoint(&checkpoint(4))?;
    assert_eq!(
        store.pending_call_ids_through(
            &session.id,
            store.message_seq(&session.id, &history[4].id)?
        )?,
        vec![call().id]
    );
    assert!(store.pending_effects(&session.id)?.is_empty());
    Ok(())
}
