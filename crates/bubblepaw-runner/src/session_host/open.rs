//! Session lookups a host command makes before it runs. The SQLite reads and
//! writes run on the blocking pool, so the async runtime never waits on them.
use anyhow::Result;
use bubblepaw_core::{Session, Store};
use std::path::Path;

/// Ids of the root sessions bound to `workspace`, found on the blocking pool.
pub(super) async fn workspace_roots(store: &Store, workspace: &Path) -> Result<Vec<String>> {
    let (listing, root) = (store.clone(), workspace.to_path_buf());
    tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
        let mut roots = Vec::new();
        for session in listing.list_sessions()? {
            if session.parent_session_id.is_none()
                && listing.session_workspace(&session.id)?.as_deref() == Some(root.as_path())
            {
                roots.push(session.id);
            }
        }
        Ok(roots)
    })
    .await?
}

/// Opens the named session, or creates one with `create`, on the blocking pool.
pub(super) async fn open_or_create(
    store: &Store,
    id: Option<&str>,
    create: impl FnOnce(&Store) -> Result<Session> + Send + 'static,
) -> Result<Session> {
    let (store, id) = (store.clone(), id.map(str::to_owned));
    tokio::task::spawn_blocking(move || match id {
        Some(id) => store.session(&id),
        None => create(&store),
    })
    .await?
}
