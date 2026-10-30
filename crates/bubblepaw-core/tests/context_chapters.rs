mod support;

use anyhow::Result;
use bubblepaw_core::{ContextChapter, Message, RunOptions, Store, Usage};
use support::harness;

fn chapter(session: &str, ordinal: usize, range: &[Message]) -> ContextChapter {
    ContextChapter {
        id: format!("{session}:chapter:{ordinal}"),
        session_id: session.into(),
        ordinal,
        first_message_id: range.first().unwrap().id.clone(),
        last_message_id: range.last().unwrap().id.clone(),
        title: format!("Chapter {ordinal}"),
        summary: format!("Preserved decision {ordinal}."),
        model: "summary-model".into(),
        usage: Some(Usage {
            input_tokens: 20,
            output_tokens: 4,
            ..Default::default()
        }),
        created_at: range.last().unwrap().created_at.clone(),
    }
}

fn durable_json(raw: &str) -> Result<serde_json::Value> {
    let mut value: serde_json::Value = serde_json::from_str(raw)?;
    value.as_object_mut().unwrap().remove("usage");
    Ok(value)
}

#[tokio::test]
async fn chapters_append_contiguously_across_foreign_seq_gaps_restart_and_fork() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("archive.sqlite");
    let store = Store::open(&path)?;
    assert_eq!(store.storage_stats()?.chat_transaction_count, 0);
    let session = store.create_session("source")?;
    let foreign =
        store.create_session_with_parent("foreign", Default::default(), Some(&session.id))?;
    let agent = harness(&store);
    for index in 0..3 {
        agent
            .prompt(&session.id, format!("input {index}"), RunOptions::default())
            .await?;
        agent
            .prompt(&foreign.id, "foreign", RunOptions::default())
            .await?;
    }
    let history = store.messages(&session.id)?;
    let original = serde_json::to_value(&history)?;
    let before = store.storage_stats()?.chat_transaction_count;
    assert!(store.context_chapters(&session.id)?.is_empty());
    let mut serialized = Vec::new();
    for ordinal in 1..=3 {
        let saved = chapter(
            &session.id,
            ordinal,
            &history[(ordinal - 1) * 2..ordinal * 2],
        );
        store.save_context_chapter(&saved)?;
        serialized.push(serde_json::to_string(&saved)?);
        let conn = rusqlite::Connection::open(store.chat_path(&session.id)?.unwrap())?;
        let mut statement =
            conn.prepare("SELECT data FROM context_chapters WHERE session_id=?1 ORDER BY ordinal")?;
        let persisted = statement
            .query_map([&session.id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(
            persisted
                .iter()
                .map(|raw| durable_json(raw))
                .collect::<Result<Vec<_>>>()?,
            serialized
                .iter()
                .map(|raw| durable_json(raw))
                .collect::<Result<Vec<_>>>()?
        );
    }
    assert_eq!(store.storage_stats()?.chat_transaction_count, before + 3);
    for _ in 0..5 {
        assert_eq!(store.context_chapters(&session.id)?.len(), 3);
    }
    assert!(store.context_chapters(&foreign.id)?.is_empty());
    assert_eq!(store.storage_stats()?.chat_transaction_count, before + 3);
    assert_eq!(
        serde_json::to_value(store.messages(&session.id)?)?,
        original
    );
    let fork = store.fork(&session.id, "fork")?;
    assert!(store.context_chapters(&fork.id)?.is_empty());
    assert!(store.context_checkpoint(&fork.id)?.is_none());
    let fork_messages = store.messages(&fork.id)?;
    assert_eq!(fork_messages.len(), history.len());
    for (copied, source) in fork_messages.iter().zip(&history) {
        assert_ne!(copied.id, source.id);
        assert_eq!(copied.content, source.content);
    }
    drop(agent);
    drop(store);
    let reopened = Store::open(&path)?;
    assert_eq!(reopened.storage_stats()?.chat_transaction_count, 0);
    assert_eq!(
        serde_json::to_value(reopened.messages(&session.id)?)?,
        original
    );
    assert_eq!(
        reopened
            .context_chapters(&session.id)?
            .iter()
            .map(serde_json::to_string)
            .collect::<serde_json::Result<Vec<_>>>()?,
        serialized
    );
    assert!(reopened.context_chapters(&fork.id)?.is_empty());
    let conn = rusqlite::Connection::open(reopened.chat_path(&session.id)?.unwrap())?;
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?,
        12
    );
    let mut statement = conn.prepare("PRAGMA foreign_key_list(context_chapters)")?;
    let references = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    assert!(references.contains(&("messages".into(), "first_message_seq".into(), "seq".into())));
    assert!(references.contains(&("messages".into(), "last_message_seq".into(), "seq".into())));
    Ok(())
}

#[tokio::test]
async fn invalid_ranges_metadata_and_duplicate_ids_are_atomic() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("source")?;
    let foreign =
        store.create_session_with_parent("foreign", Default::default(), Some(&session.id))?;
    let agent = harness(&store);
    for _ in 0..3 {
        agent
            .prompt(&session.id, "input", RunOptions::default())
            .await?;
    }
    agent
        .prompt(&foreign.id, "other", RunOptions::default())
        .await?;
    let history = store.messages(&session.id)?;
    let foreign_history = store.messages(&foreign.id)?;
    let first = chapter(&session.id, 1, &history[..2]);
    let mut gap_at_start = first.clone();
    gap_at_start.first_message_id = history[1].id.clone();
    let before = store.storage_stats()?.chat_transaction_count;
    assert!(store.save_context_chapter(&gap_at_start).is_err());
    assert!(store.context_chapters(&session.id)?.is_empty());
    assert_eq!(store.storage_stats()?.chat_transaction_count, before);
    store.save_context_chapter(&first)?;
    let baseline = serde_json::to_value(store.context_chapters(&session.id)?)?;
    let original = serde_json::to_value(&history)?;
    let before = store.storage_stats()?.chat_transaction_count;
    let next = chapter(&session.id, 2, &history[2..4]);
    let reject = |bad: &ContextChapter| -> Result<()> {
        assert!(
            store.save_context_chapter(bad).is_err(),
            "accepted invalid chapter: {bad:?}"
        );
        assert_eq!(store.storage_stats()?.chat_transaction_count, before);
        assert_eq!(
            serde_json::to_value(store.context_chapters(&session.id)?)?,
            baseline
        );
        assert_eq!(
            serde_json::to_value(store.messages(&session.id)?)?,
            original
        );
        assert!(store.context_chapters(&foreign.id)?.is_empty());
        Ok(())
    };
    for ordinal in [0, 1, 3, usize::MAX] {
        let mut bad = next.clone();
        bad.ordinal = ordinal;
        reject(&bad)?;
    }
    for first_id in [
        &history[0].id,
        &history[1].id,
        &history[3].id,
        &foreign_history[0].id,
        &"missing".into(),
        &" ".into(),
    ] {
        let mut bad = next.clone();
        bad.first_message_id = first_id.clone();
        reject(&bad)?;
    }
    for last_id in [
        &history[1].id,
        &foreign_history[1].id,
        &"missing".into(),
        &" ".into(),
    ] {
        let mut bad = next.clone();
        bad.last_message_id = last_id.clone();
        reject(&bad)?;
    }
    for title in [
        " ".into(),
        "x".repeat(81),
        "first\nsecond".into(),
        "first\rsecond".into(),
    ] {
        let mut bad = next.clone();
        bad.title = title;
        reject(&bad)?;
    }
    for summary in [" ".into(), "x".repeat(65537), "é".repeat(32769)] {
        let mut bad = next.clone();
        bad.summary = summary;
        reject(&bad)?;
    }
    for id in [" ".into(), first.id.clone()] {
        let mut bad = next.clone();
        bad.id = id;
        reject(&bad)?;
    }
    for session_id in [" ", "missing", foreign.id.as_str()] {
        let mut bad = next.clone();
        bad.session_id = session_id.into();
        reject(&bad)?;
    }
    let mut bad = next.clone();
    bad.model = " ".into();
    reject(&bad)?;
    bad = next.clone();
    bad.created_at.clear();
    reject(&bad)?;
    reject(&first)?;
    let mut valid = next;
    valid.title = "é".repeat(80);
    valid.summary = "é".repeat(32768);
    store.save_context_chapter(&valid)?;
    assert_eq!(store.storage_stats()?.chat_transaction_count, before + 1);
    assert_eq!(store.context_chapters(&session.id)?[1].summary.len(), 65536);
    // A single-message chapter is still a contiguous, nonempty range.
    store.save_context_chapter(&chapter(&session.id, 3, &history[4..5]))?;
    store.save_context_chapter(&chapter(&session.id, 4, &history[5..6]))?;
    Ok(())
}
