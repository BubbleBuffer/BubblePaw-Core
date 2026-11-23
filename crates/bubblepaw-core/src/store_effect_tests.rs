//! Effect lookup must preserve unrelated cold storage.
use super::*;

fn call() -> ToolCall {
    ToolCall {
        id: id(),
        name: "external-effect".into(),
        arguments: serde_json::json!({}),
    }
}

fn state(store: &Store, invocation: &str) -> Result<String> {
    store.read(|conn| {
        Ok(conn.query_row(
            "SELECT state FROM effects WHERE id=?1",
            [invocation],
            |row| row.get(0),
        )?)
    })
}

fn archived_chat(store: &Store) -> Result<(Session, PathBuf, Vec<u8>)> {
    let session = store.create_session("unrelated archive")?;
    let path = store.chat_path(&session.id)?.unwrap();
    assert!(store.archive_chat(&session.id)?);
    let archive = cold::archive_path(&path);
    let bytes = std::fs::read(&archive)?;
    Ok((session, path, bytes))
}

fn assert_archived(store: &Store, session: &str, path: &Path, bytes: &[u8]) -> Result<()> {
    assert!(!path.exists(), "effect lookup restored an unrelated chat");
    assert_eq!(std::fs::read(cold::archive_path(path))?, bytes);
    assert!(store.active_session_metadata(session)?.is_none());
    Ok(())
}

#[test]
fn missing_effect_lookups_preserve_archived_chats() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let (archived, path, bytes) = archived_chat(&store)?;
    assert!(store.effect_completed("missing-completion").is_err());
    assert_archived(&store, &archived.id, &path, &bytes)?;
    assert!(store.effect_unknown("missing-unknown").is_err());
    assert_archived(&store, &archived.id, &path, &bytes)?;
    Ok(())
}

#[test]
fn active_child_effect_lookup_preserves_newer_archived_chat() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let root = store.create_session("active root")?;
    let child = store.create_session_with_parent(
        "active child",
        ContextStrategy::default(),
        Some(&root.id),
    )?;
    let completed = store.dispatch_nested(&child.id, "run", &call())?;
    let uncertain = store.dispatch_nested(&child.id, "run", &call())?;
    let (archived, path, bytes) = archived_chat(&store)?;
    // Force the unrelated archive to precede the owner in the old global scan.
    store
        .0
        .layout
        .as_ref()
        .unwrap()
        .catalog
        .lock()
        .unwrap()
        .execute(
            "UPDATE sessions SET last_activity='9999-01-01T00:00:00Z' WHERE id=?1",
            [&archived.id],
        )?;
    store.effect_completed(&completed)?;
    assert_archived(&store, &archived.id, &path, &bytes)?;
    store.effect_unknown(&uncertain)?;
    assert_archived(&store, &archived.id, &path, &bytes)?;
    let owner = store.scoped(&child.id)?.unwrap();
    assert_eq!(state(&owner, &completed)?, "completed");
    assert_eq!(state(&owner, &uncertain)?, "unknown");
    Ok(())
}
