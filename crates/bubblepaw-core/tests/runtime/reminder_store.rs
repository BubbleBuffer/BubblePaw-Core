use anyhow::Result;
use bubblepaw_core::{InputKind, Message, MessageOrigin, Reminder, ReminderMetadata, Store};
use rusqlite::{Connection, params};
use serde_json::json;

fn reminder(kind: &str, text: &str) -> Reminder {
    Reminder {
        kind: kind.into(),
        text: text.into(),
        deliveries: Vec::new(),
    }
}

fn message(session: &str, id: &str) -> Result<Message> {
    Ok(serde_json::from_value(json!({
        "id": id, "session_id": session, "role": "user",
        "origin": "generated", "content": "<system-reminder>combined</system-reminder>",
        "created_at": "2026-10-21T00:00:00Z"
    }))?)
}

#[test]
fn accepted_reminder_text_and_metadata_share_one_write_and_survive_reopen() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("paw.db");
    let store = Store::open(&path)?;
    let session = store.create_session("reminders")?;
    let other = store.create_session("other")?;
    let before = store.storage_stats()?.chat_transaction_count;
    let input = store.queue_reminder(
        &session.id,
        InputKind::Steer,
        reminder("memory", "original raw reminder"),
    )?;
    assert_eq!(store.storage_stats()?.chat_transaction_count, before + 1);
    assert_eq!(input.text, "original raw reminder");
    assert_eq!(input.origin, Some(MessageOrigin::Generated));
    assert_eq!(store.queued_inputs(&session.id)?[0].text, input.text);
    let metadata = store.reminder_metadata(&session.id, &input.id)?.unwrap();
    assert_eq!(metadata.version, 1);
    assert_eq!(metadata.request_kind, "root");
    assert_eq!(metadata.reminders[0].kind, "memory");
    assert_eq!(
        metadata.reminders[0].text_sha256,
        bubblepaw_core::fingerprint(input.text.as_bytes())
    );
    assert!(store.reminder_metadata(&other.id, &input.id)?.is_none());
    assert!(!store.reminder_legacy_eligible(&session.id, &input.id)?);
    assert!(store.last_reminders(&session.id, "root")?.is_empty());
    drop(store);
    let reopened = Store::open(&path)?;
    assert_eq!(
        serde_json::to_value(reopened.reminder_metadata(&session.id, &input.id)?)?,
        serde_json::to_value(Some(metadata))?
    );
    assert_eq!(reopened.queued_inputs(&session.id)?[0].text, input.text);
    Ok(())
}

#[test]
fn missing_metadata_does_not_make_current_inputs_or_messages_legacy() -> Result<()> {
    let directory = tempfile::tempdir()?;
    for store in [
        Store::in_memory()?,
        Store::open(directory.path().join("paw.db"))?,
    ] {
        let session = store.create_session("current")?;
        let input = store.queue(
            &session.id,
            InputKind::Followup,
            "<system-reminder>typed</system-reminder>",
        )?;
        let ordinary = message(&session.id, "ordinary")?;
        store.append_batch(std::slice::from_ref(&ordinary))?;
        assert!(store.reminder_metadata(&session.id, &input.id)?.is_none());
        assert!(
            store
                .reminder_metadata(&session.id, &ordinary.id)?
                .is_none()
        );
        assert!(!store.reminder_legacy_eligible(&session.id, &input.id)?);
        assert!(!store.reminder_legacy_eligible(&session.id, &ordinary.id)?);
        assert!(!store.reminder_legacy_eligible(&session.id, "absent")?);
    }
    Ok(())
}

#[test]
fn migration_cutoffs_are_frozen_across_restart() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("paw.db");
    let store = Store::open(&path)?;
    let session = store.create_session("old")?;
    let old_input = store.queue(&session.id, InputKind::Steer, "legacy generated")?;
    let old_message = message(&session.id, "legacy-message")?;
    store.append_batch(std::slice::from_ref(&old_message))?;
    let chat = store.chat_path(&session.id)?.unwrap();
    drop(store);
    // Simulate the exact pre-additive shape without regressing other schema.
    let conn = Connection::open(&chat)?;
    conn.execute_batch("DROP TABLE reminder_metadata; DELETE FROM metadata WHERE key IN ('reminders_v1_message_cutoff','reminders_v1_input_cutoff');")?;
    drop(conn);
    let store = Store::open(&path)?;
    assert!(store.reminder_legacy_eligible(&session.id, &old_input.id)?);
    assert!(store.reminder_legacy_eligible(&session.id, &old_message.id)?);
    let new_input = store.queue(&session.id, InputKind::Followup, "new ordinary")?;
    let new_message = message(&session.id, "new-message")?;
    store.append_batch(std::slice::from_ref(&new_message))?;
    assert!(!store.reminder_legacy_eligible(&session.id, &new_input.id)?);
    assert!(!store.reminder_legacy_eligible(&session.id, &new_message.id)?);
    drop(store);
    let store = Store::open(&path)?;
    assert!(store.reminder_legacy_eligible(&session.id, &old_message.id)?);
    assert!(!store.reminder_legacy_eligible(&session.id, &new_message.id)?);
    assert!(!store.reminder_legacy_eligible(&session.id, &new_input.id)?);
    Ok(())
}

#[test]
fn latest_sent_reminders_restore_per_kind_and_request_lineage() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("paw.db");
    let store = Store::open(&path)?;
    let session = store.create_session("latest")?;
    for (id, request_kind, reminders) in [
        (
            "one",
            "root",
            vec![reminder("memory", "old"), reminder("skills", "unchanged")],
        ),
        ("two", "fork:1", vec![reminder("memory", "fork-only")]),
        ("three", "root", vec![reminder("memory", "new")]),
    ] {
        store.append_batch(&[message(&session.id, id)?])?;
        let conn = Connection::open(store.chat_path(&session.id)?.unwrap())?;
        let metadata = ReminderMetadata::new(request_kind, reminders);
        conn.execute(
            "INSERT INTO reminder_metadata(session_id,record_id,data) VALUES(?1,?2,?3)",
            params![session.id, id, serde_json::to_string(&metadata)?],
        )?;
    }
    // Accepted but unsent values must never seed sent-value deduplication.
    store.queue_reminder(&session.id, InputKind::Steer, reminder("memory", "unsent"))?;
    drop(store);
    let store = Store::open(&path)?;
    let mut root = store.last_reminders(&session.id, "root")?;
    root.sort_by(|a, b| a.kind.cmp(&b.kind));
    assert_eq!(root.len(), 2);
    assert_eq!(root[0].kind, "memory");
    assert_eq!(root[0].text_sha256, bubblepaw_core::fingerprint(b"new"));
    assert_eq!(root[1].kind, "skills");
    assert_eq!(
        root[1].text_sha256,
        bubblepaw_core::fingerprint(b"unchanged")
    );
    let fork = store.last_reminders(&session.id, "fork:1")?;
    assert_eq!(fork.len(), 1);
    assert_eq!(
        fork[0].text_sha256,
        bubblepaw_core::fingerprint(b"fork-only")
    );
    assert!(store.last_reminders(&session.id, "unknown")?.is_empty());
    Ok(())
}
