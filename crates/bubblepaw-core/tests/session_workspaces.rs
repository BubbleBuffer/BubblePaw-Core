use anyhow::{Context, Result};
use bubblepaw_core::{ContextStrategy, Store};
use rusqlite::{Connection, params};
use std::path::Path;

#[test]
fn root_workspaces_are_atomic_distinct_and_inherited_through_rebuild() -> Result<()> {
    let data = tempfile::tempdir()?;
    let a = tempfile::tempdir()?;
    let b = tempfile::tempdir()?;
    let store = Store::open_data_dir(data.path())?;
    store.bind_workspace(a.path())?;
    let root_a = store.create_session_with_workspace("a", a.path())?;
    let root_b = store.create_session_with_strategy_and_workspace(
        "b",
        ContextStrategy::default(),
        b.path(),
    )?;
    let child =
        store.create_session_with_parent("child", ContextStrategy::default(), Some(&root_b.id))?;
    let grandchild = store.create_session_with_parent(
        "grandchild",
        ContextStrategy::default(),
        Some(&child.id),
    )?;
    assert_eq!(
        store.session_workspace(&root_a.id)?,
        Some(a.path().canonicalize()?)
    );
    for session in [&root_b.id, &child.id, &grandchild.id] {
        assert_eq!(
            store.session_workspace(session)?,
            Some(b.path().canonicalize()?)
        );
    }
    assert!(
        store
            .create_session_with_workspace("missing", data.path().join("missing"))
            .is_err()
    );
    let file = data.path().join("not-a-directory");
    std::fs::write(&file, "fixture")?;
    assert!(store.create_session_with_workspace("file", &file).is_err());
    assert_eq!(store.list_sessions()?.len(), 4);
    let stats = store.storage_stats()?;
    store.catalog_rebuild()?;
    assert_eq!(
        store.storage_stats()?.chat_transaction_count,
        stats.chat_transaction_count
    );
    assert_eq!(store.bound_workspace()?, Some(a.path().canonicalize()?));
    drop(store);
    // A disposable catalog loss must not select one chat's root for another.
    std::fs::remove_file(data.path().join("catalog.db"))?;
    let reopened = Store::open_data_dir(data.path())?;
    assert_eq!(reopened.bound_workspace()?, None);
    assert_eq!(
        reopened.session_workspace(&root_b.id)?,
        Some(b.path().canonicalize()?)
    );
    assert_eq!(
        reopened.session_workspace(&grandchild.id)?,
        Some(b.path().canonicalize()?)
    );
    assert_eq!(reopened.storage_stats()?.chat_transaction_count, 0);
    Ok(())
}

fn remove_new_binding(path: &Path, root: &str, legacy: Option<&Path>) -> Result<()> {
    let connection = Connection::open(path)?;
    connection.execute(
        "DELETE FROM metadata WHERE key=?1",
        [format!("root_workspace:{root}")],
    )?;
    if let Some(workspace) = legacy {
        connection.execute(
            "INSERT OR REPLACE INTO metadata VALUES('workspace',?1)",
            [workspace.to_string_lossy().as_ref()],
        )?;
    }
    connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    Ok(())
}

#[test]
fn old_chat_and_catalog_fallbacks_open_without_rewriting_chat_files() -> Result<()> {
    let data = tempfile::tempdir()?;
    let fallback = tempfile::tempdir()?;
    let old_workspace = tempfile::tempdir()?;
    let store = Store::open_data_dir(data.path())?;
    store.bind_workspace(fallback.path())?;
    let old = store
        .create_session("old workspace metadata")
        .context("create old fixture")?;
    let catalog_only = store
        .create_session("catalog fallback only")
        .context("create catalog fixture")?;
    let old_path = store.chat_path(&old.id)?.unwrap();
    let catalog_path = store.chat_path(&catalog_only.id)?.unwrap();
    drop(store);
    remove_new_binding(
        &old_path,
        &old.id,
        Some(&old_workspace.path().canonicalize()?),
    )?;
    remove_new_binding(&catalog_path, &catalog_only.id, None)?;
    // Legacy migration kept fork ancestry while placing each historical
    // session in its own chat. That parent need not be in this record file.
    let legacy = Connection::open(&old_path)?;
    let mut fork = old.clone();
    fork.parent_session_id = Some(catalog_only.id.clone());
    legacy.execute(
        "UPDATE sessions SET data=?2 WHERE id=?1",
        params![fork.id, serde_json::to_string(&fork)?],
    )?;
    legacy.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    drop(legacy);
    let before_old = std::fs::read(&old_path)?;
    let before_catalog = std::fs::read(&catalog_path)?;
    let reopened = Store::open_data_dir(data.path()).context("reopen old fixture")?;
    assert_eq!(reopened.session_root_id(&old.id)?, old.id);
    assert_eq!(reopened.session_root_id(&catalog_only.id)?, catalog_only.id);
    assert_eq!(
        reopened
            .session_workspace(&old.id)
            .context("old chat workspace")?,
        Some(old_workspace.path().canonicalize()?)
    );
    assert_eq!(
        reopened
            .session_workspace(&catalog_only.id)
            .context("catalog fallback workspace")?,
        Some(fallback.path().canonicalize()?)
    );
    reopened.catalog_rebuild()?;
    assert_eq!(reopened.storage_stats()?.chat_transaction_count, 0);
    drop(reopened);
    assert_eq!(std::fs::read(old_path)?, before_old);
    assert_eq!(std::fs::read(catalog_path)?, before_catalog);
    Ok(())
}

#[test]
fn session_workspace_reads_do_not_restore_archived_chats() -> Result<()> {
    let data = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let store = Store::open_data_dir(data.path())?;
    let root = store.create_session_with_workspace("archived", workspace.path())?;
    let path = store.chat_path(&root.id)?.unwrap();
    assert!(store.archive_chat(&root.id)?);
    assert!(!path.exists());
    let stats = store.storage_stats()?;
    assert_eq!(store.session_root_id(&root.id)?, root.id);
    assert_eq!(
        store.session_workspace(&root.id)?,
        Some(workspace.path().canonicalize()?)
    );
    assert!(!path.exists());
    assert_eq!(
        store.storage_stats()?.chat_transaction_count,
        stats.chat_transaction_count
    );
    Ok(())
}

#[test]
fn catalog_rebuild_rejects_only_the_chat_with_a_malformed_binding() -> Result<()> {
    for key_kind in ["root", "legacy", "foreign-root"] {
        let data = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let store = Store::open_data_dir(data.path())?;
        let root = store.create_session_with_workspace("malformed", workspace.path())?;
        let healthy = store.create_session_with_workspace("healthy", workspace.path())?;
        let path = store.chat_path(&root.id)?.unwrap();
        drop(store);
        let connection = Connection::open(&path)?;
        let key = match key_kind {
            "root" => format!("root_workspace:{}", root.id),
            "legacy" => "workspace".to_owned(),
            _ => "root_workspace:unrelated-root".to_owned(),
        };
        connection.execute(
            "INSERT OR REPLACE INTO metadata VALUES(?1,?2)",
            params![key, "relative/workspace"],
        )?;
        connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        drop(connection);
        let before = std::fs::read(&path)?;
        // The store opens; only the malformed chat is rejected.
        let reopened = Store::open_data_dir(data.path())
            .with_context(|| format!("malformed {key_kind} binding blocked the store"))?;
        let listed: Vec<String> = reopened
            .list_sessions()?
            .into_iter()
            .map(|session| session.id)
            .collect();
        assert_eq!(listed, vec![healthy.id.clone()], "{key_kind}");
        assert!(reopened.session(&root.id).is_err(), "{key_kind}");
        assert_eq!(
            reopened.session_workspace(&healthy.id)?,
            Some(workspace.path().canonicalize()?)
        );
        let added = reopened.create_session("after")?;
        assert!(reopened.session(&added.id).is_ok());
        reopened.catalog_rebuild()?;
        drop(reopened);
        assert_eq!(
            std::fs::read(&path)?,
            before,
            "{key_kind} chat was rewritten"
        );
    }
    Ok(())
}

#[test]
fn stored_windows_and_unix_roots_remain_readable_across_platforms() -> Result<()> {
    for workspace in [
        r"C:\work\repo",
        r"\\server\share\repo",
        r"\\?\C:\work\repo",
        "/old/unix/repo",
    ] {
        let data = tempfile::tempdir()?;
        let store = Store::open_data_dir(data.path())?;
        let root = store.create_session("portable old record")?;
        let path = store.chat_path(&root.id)?.unwrap();
        drop(store);
        let connection = Connection::open(&path)?;
        connection.execute("INSERT INTO metadata VALUES('workspace',?1)", [workspace])?;
        connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        drop(connection);
        let before = std::fs::read(&path)?;
        let reopened = Store::open_data_dir(data.path())?;
        assert_eq!(
            reopened.session_workspace(&root.id)?,
            Some(workspace.into())
        );
        reopened.catalog_rebuild()?;
        drop(reopened);
        assert_eq!(std::fs::read(path)?, before);
    }
    Ok(())
}

#[test]
fn in_memory_roots_use_the_same_workspace_contract() -> Result<()> {
    let store = Store::in_memory()?;
    let a = tempfile::tempdir()?;
    let b = tempfile::tempdir()?;
    store.bind_workspace(a.path())?;
    let root = store.create_session_with_workspace("b", b.path())?;
    let child =
        store.create_session_with_parent("child", ContextStrategy::default(), Some(&root.id))?;
    assert_eq!(
        store.session_workspace(&child.id)?,
        Some(b.path().canonicalize()?)
    );
    assert_eq!(store.bound_workspace()?, Some(a.path().canonicalize()?));
    assert!(store.session_workspace("unknown").is_err());
    Ok(())
}
