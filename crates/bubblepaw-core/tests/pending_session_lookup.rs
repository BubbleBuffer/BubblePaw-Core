//! A child session is listed from the pending catalog buffer as soon as its
//! record commits, but its catalog row only lands when the buffer is flushed.
//! Resolving the listed session's workspace in that window must not fail with
//! "Unknown session" (it made `session.list` error while a child was starting).
use anyhow::Result;
use bubblepaw_core::{ContextStrategy, Store};
use rusqlite::Connection;

#[test]
fn a_listed_child_resolves_its_workspace_before_its_catalog_row_lands() -> Result<()> {
    let data = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let store = Store::open_data_dir(data.path())?;
    let root = store.create_session_with_workspace("root", workspace.path())?;
    let child =
        store.create_session_with_parent("child", ContextStrategy::default(), Some(&root.id))?;
    // Put the catalog back in the state of the window: no row for the child.
    {
        let catalog = Connection::open(data.path().join("catalog.db"))?;
        catalog.busy_timeout(std::time::Duration::from_secs(10))?;
        catalog.execute("DELETE FROM sessions WHERE id=?1", [&child.id])?;
    }
    // A committed record refreshes the pending buffer, which then holds the child.
    store.store_attachment(&root.id, "probe", "{}", b"x")?;
    assert!(store.list_sessions()?.iter().any(|s| s.id == child.id));
    assert_eq!(
        store.session_workspace(&child.id)?,
        Some(workspace.path().canonicalize()?)
    );
    assert!(store.session_storage_policy(&child.id).is_ok());
    Ok(())
}
