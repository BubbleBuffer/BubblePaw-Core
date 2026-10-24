use super::*;
use crate::InputKind;

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
