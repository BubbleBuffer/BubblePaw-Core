//! SQLITE_BUSY regression: the buffered serving transaction keeps a read snapshot
//! of the attached catalog.db and cannot upgrade it once another connection has
//! written (SQLITE_BUSY_SNAPSHOT, 517). Session creation therefore never writes
//! catalog.db inside the buffer.

use super::*;

#[test]
fn session_creation_after_a_catalog_commit_does_not_fail_busy() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&workspace)?;
    // A buffered operation reads the catalog and leaves the buffer open, so the
    // serving connection holds a read snapshot of catalog.db.
    store.write_serving(|conn| {
        conn.query_row(
            "SELECT COUNT(*) FROM catalog_projection.metadata",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(())
    })?;
    // The catalog connection commits a write: the snapshot is now stale.
    store.bind_workspace(&workspace)?;
    let session = store.create_session("after a catalog commit")?;
    store.flush_serving()?;
    assert_eq!(store.session(&session.id)?.title, "after a catalog commit");
    Ok(())
}
