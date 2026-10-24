use super::*;
use crate::LegacyImportStatus;

#[test]
fn new_sessions_bind_once_and_legacy_missing_modes_remain_js() -> Result<()> {
    let store = Store::in_memory()?;
    let fresh = store.create_session("fresh")?;
    let before = store.revision();
    assert_eq!(
        store.resolve_execution_mode(&fresh.id, ExecutionMode::Native)?,
        ExecutionMode::Native
    );
    assert_eq!(store.revision() - before, 1);
    let bound = store.revision();
    assert_eq!(
        store.resolve_execution_mode(&fresh.id, ExecutionMode::Js)?,
        ExecutionMode::Native
    );
    assert_eq!(store.revision(), bound, "a bound mode is read-only");
    let legacy = store.create_session("legacy empty chat")?;
    store.write(|tx| {
        tx.execute("DELETE FROM metadata WHERE key=?1", [key(&legacy.id)])?;
        Ok(())
    })?;
    let before = store.revision();
    assert_eq!(
        store.resolve_execution_mode(&legacy.id, ExecutionMode::Native)?,
        ExecutionMode::Js
    );
    assert_eq!(
        store.revision(),
        before,
        "legacy fallback needs no migration write"
    );
    assert_eq!(store.chat_metadata(&legacy.id, &key(&legacy.id))?, None);
    Ok(())
}

#[test]
fn persisted_modes_survive_restart_children_forks_and_serving_rebuild() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let native;
    let js;
    {
        let store = Store::open_data_dir(dir.path())?;
        native = store.create_session("native")?.id;
        js = store.create_session("js")?.id;
        assert_eq!(
            store.resolve_execution_mode(&native, ExecutionMode::Native)?,
            ExecutionMode::Native
        );
        assert_eq!(
            store.resolve_execution_mode(&js, ExecutionMode::Js)?,
            ExecutionMode::Js
        );
        store.serving_wipe()?;
    }
    let store = Store::open_data_dir(dir.path())?;
    for (parent, saved, configured) in [
        (&native, ExecutionMode::Native, ExecutionMode::Js),
        (&js, ExecutionMode::Js, ExecutionMode::Native),
    ] {
        assert_eq!(store.resolve_execution_mode(parent, configured)?, saved);
        let child = store.create_session_with_parent("child", Default::default(), Some(parent))?;
        assert_eq!(store.resolve_execution_mode(&child.id, configured)?, saved);
        let fork = store.fork(parent, "fork")?;
        assert_eq!(store.resolve_execution_mode(&fork.id, configured)?, saved);
        assert!(store.chat_metadata(&fork.id, &key(&fork.id))?.is_some());
    }
    Ok(())
}

#[test]
fn flat_store_forks_preserve_mode() -> Result<()> {
    let store = Store::in_memory()?;
    let source = store.create_session("native")?;
    store.resolve_execution_mode(&source.id, ExecutionMode::Native)?;
    let fork = store.fork(&source.id, "fork")?;
    assert_eq!(
        store.resolve_execution_mode(&fork.id, ExecutionMode::Js)?,
        ExecutionMode::Native
    );
    Ok(())
}

#[test]
fn invalid_persisted_mode_is_reported() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("invalid")?;
    store.set_chat_metadata(&key(&session.id), "broken")?;
    assert!(
        store
            .resolve_execution_mode(&session.id, ExecutionMode::Native)
            .unwrap_err()
            .to_string()
            .contains("Invalid persisted execution mode")
    );
    Ok(())
}

#[test]
fn legacy_import_preserves_bound_modes_and_missing_mode_fallback() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let base = dir.path().join("BubblePaw-base");
    let legacy_path = base.join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let native;
    let legacy;
    {
        let store = Store::open_data_dir(&legacy_path)?;
        store.bind_workspace(dir.path())?;
        native = store.create_session("native source")?.id;
        store.resolve_execution_mode(&native, ExecutionMode::Native)?;
        legacy = store.create_session("legacy without mode")?.id;
        let chat = store.scoped(&legacy)?.unwrap();
        chat.write(|tx| {
            tx.execute("DELETE FROM metadata WHERE key=?1", [key(&legacy)])?;
            Ok(())
        })?;
    }
    let central = Store::open_data_dir(base.join("store"))?;
    let report = central.import_legacy_stores(|_| Ok(()))?;
    assert_eq!(report.sources.len(), 1);
    assert_eq!(report.sources[0].status, LegacyImportStatus::Imported);
    assert_eq!(
        central.resolve_execution_mode(&native, ExecutionMode::Js)?,
        ExecutionMode::Native
    );
    assert_eq!(
        central.resolve_execution_mode(&legacy, ExecutionMode::Native)?,
        ExecutionMode::Js
    );
    Ok(())
}

#[test]
fn deleting_child_subtrees_removes_their_mode_metadata_only() -> Result<()> {
    fn check(store: Store) -> Result<()> {
        let root = store.create_session("retained root")?;
        store.resolve_execution_mode(&root.id, ExecutionMode::Native)?;
        let child = store.create_session_with_parent(
            "deleted child",
            Default::default(),
            Some(&root.id),
        )?;
        let grandchild = store.create_session_with_parent(
            "deleted grandchild",
            Default::default(),
            Some(&child.id),
        )?;
        let retained = store.create_session_with_parent(
            "retained sibling",
            Default::default(),
            Some(&root.id),
        )?;
        for session in [&root.id, &child.id, &grandchild.id, &retained.id] {
            assert_eq!(
                store.chat_metadata(&root.id, &key(session))?.as_deref(),
                Some("native")
            );
        }
        store.delete_session_preserving_effects(&child.id)?;
        for deleted in [&child.id, &grandchild.id] {
            assert_eq!(store.chat_metadata(&root.id, &key(deleted))?, None);
            assert!(store.session(deleted).is_err());
        }
        for kept in [&root.id, &retained.id] {
            assert_eq!(
                store.resolve_execution_mode(kept, ExecutionMode::Js)?,
                ExecutionMode::Native
            );
        }
        Ok(())
    }
    check(Store::in_memory()?)?;
    let dir = tempfile::tempdir()?;
    check(Store::open_data_dir(dir.path())?)
}
