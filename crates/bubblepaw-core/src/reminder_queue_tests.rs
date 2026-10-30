use super::*;
use crate::InputKind;

#[test]
fn snapshot_discards_only_unsent_host_facts_in_its_families() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("snapshot reminders")?.id;
    store.queue_reminder(
        &session,
        InputKind::Steer,
        Reminder::new("skills:accepted", "accepted input"),
    )?;
    store.consume(&session, true, false)?;
    let context = ContextState::default();
    context.remind(&session, "root", "skills:old", "superseded skill delta");
    context.remind_fresh(
        &session,
        "root",
        "memory_files:old",
        "superseded memory delta",
    );
    context.remind(&session, "root", "live_config:keep", "config remains");
    context.remind_with_metadata(
        &session,
        "root",
        Reminder::new("skills:delivery", "delivery remains").with_delivery("external", "id"),
    );
    context.preview_reminder_message(&session, "root", &store)?;
    crate::ContextWriter::new(&context, &session)
        .discard_pending_reminders(&["skills:", "memory_files:"]);
    let built = context.build_persisted(
        ModelRequest::default(),
        &session,
        "root",
        &[],
        &store,
        |_| Ok(()),
    )?;
    let text = &built.request.messages[0].content;
    assert!(!text.contains("superseded"));
    assert!(text.contains("accepted input"));
    assert!(text.contains("config remains"));
    assert!(text.contains("delivery remains"));
    assert_eq!(store.messages(&session)?.len(), 1);
    Ok(())
}

#[test]
fn replayed_staged_sources_cannot_overwrite_a_newer_direct_fact() -> Result<()> {
    for preview_first in [false, true] {
        let store = Store::in_memory()?;
        let session = store.create_session("ordering")?.id;
        store.queue_reminder(
            &session,
            InputKind::Steer,
            Reminder::new("inventory", "old accepted snapshot"),
        )?;
        store.consume(&session, true, false)?;
        let context = ContextState::default();
        if preview_first {
            context.preview_reminder_message(&session, "root", &store)?;
        }
        context.remind(&session, "root", "inventory", "current direct fact");
        let preview = context
            .preview_reminder_message(&session, "root", &store)?
            .unwrap();
        assert!(preview.content.contains("current direct fact"));
        let built = context.build_persisted(
            ModelRequest::default(),
            &session,
            "root",
            &[],
            &store,
            |_| Ok(()),
        )?;
        assert_eq!(built.request.messages[0].content, preview.content);
        assert!(
            !built.request.messages[0]
                .content
                .contains("old accepted snapshot")
        );
        assert_eq!(store.queued_inputs(&session)?[0].state, "consumed");
    }
    Ok(())
}

#[test]
fn coalesced_sources_are_all_acknowledged_only_with_durable_tail() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("coalesce")?.id;
    store.queue_reminder(
        &session,
        InputKind::Steer,
        Reminder::new("inventory", "first"),
    )?;
    store.queue_reminder(
        &session,
        InputKind::Steer,
        Reminder::new("inventory", "last"),
    )?;
    store.consume(&session, true, false)?;
    let context = ContextState::default();
    assert!(
        context
            .build_persisted(
                ModelRequest::default(),
                &session,
                "root",
                &[],
                &store,
                |_| Err(anyhow::anyhow!("validation failure"))
            )
            .is_err()
    );
    assert!(
        store
            .queued_inputs(&session)?
            .iter()
            .all(|input| input.state == "pending")
    );
    let built = context.build_persisted(
        ModelRequest::default(),
        &session,
        "root",
        &[],
        &store,
        |_| Ok(()),
    )?;
    assert_eq!(built.request.messages.len(), 1);
    assert_eq!(
        built.request.messages[0].content,
        "<harness kind=\"inventory\">last</harness>"
    );
    assert!(
        store
            .queued_inputs(&session)?
            .iter()
            .all(|input| input.state == "consumed")
    );
    assert_eq!(store.messages(&session)?.len(), 1);
    Ok(())
}
