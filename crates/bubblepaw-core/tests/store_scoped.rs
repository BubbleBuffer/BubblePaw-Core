//! `scoped` resolves a session's placement once per open chat, not per call.
use anyhow::Result;
use bubblepaw_core::{ContextStrategy, Message, Role, Store};

fn message(session: &str, id: &str, content: &str) -> Message {
    Message {
        id: id.into(),
        session_id: session.into(),
        role: Role::User,
        origin: None,
        content: content.into(),
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: "2026-10-12T00:00:00Z".into(),
    }
}

#[test]
fn scoped_resolves_catalog_once() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let session = store.create_session("scoped caching")?;
    let before = store.catalog_lookups();
    for i in 0..8 {
        store.append_batch(&[message(&session.id, &format!("m{i}"), "marigold camera")])?;
    }
    assert_eq!(store.messages(&session.id)?.len(), 8);
    assert_eq!(
        store.catalog_lookups() - before,
        1,
        "repeat scoped calls must reuse the resolved placement"
    );
    Ok(())
}

#[test]
fn scoped_re_resolves_after_archive_and_delete() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let session = store.create_session("scoped invalidation")?;
    store.append_batch(&[message(&session.id, "m0", "kept")])?;
    assert_eq!(store.messages(&session.id)?.len(), 1);
    assert!(store.archive_chat(&session.id)?);
    // Archiving closed the chat; the next read restores it through the catalog.
    let after_archive = store.catalog_lookups();
    assert_eq!(store.messages(&session.id)?[0].content, "kept");
    assert!(store.catalog_lookups() > after_archive);
    store.delete_session(&session.id)?;
    let error = store.messages(&session.id).unwrap_err().to_string();
    assert!(
        error.contains("Unknown session or missing chat file"),
        "{error}"
    );
    Ok(())
}

#[test]
fn scoped_forgets_a_deleted_child_in_an_open_chat() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let root = store.create_session("root")?;
    let child =
        store.create_session_with_parent("child", ContextStrategy::default(), Some(&root.id))?;
    store.append_batch(&[message(&child.id, "c0", "child")])?;
    assert_eq!(store.messages(&child.id)?.len(), 1);
    store.delete_session(&child.id)?;
    assert_eq!(store.messages(&root.id)?.len(), 0);
    // As before the cache: the deleted child reads as an error or as empty, never its rows.
    assert!(!matches!(store.messages(&child.id), Ok(rows) if !rows.is_empty()));
    Ok(())
}
