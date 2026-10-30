//! Session lookups a host command makes before it runs. The SQLite reads and
//! writes run on the blocking pool, so the async runtime never waits on them.
use super::*;
use bubblepaw_core::Store;
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

pub(super) fn providers(
    effective: &EffectiveConfig,
    model_provider: &Arc<LiveProviderConfig>,
    live_config: &Arc<LiveConfig>,
) -> Result<ProviderSetup> {
    Ok(ProviderSetup::new(effective.load_options(), effective)?
        .with_live(model_provider.clone())
        .with_host_config(live_config.clone()))
}

/// Opens the named session, or creates one titled `name` (or `default`).
pub(super) async fn open_session(
    ctx: &HostCtx,
    session: Option<&str>,
    name: Option<&str>,
    default: &str,
) -> Result<String> {
    let title = match session {
        Some(_) => None,
        None => Some(session_title(name, default)?),
    };
    let params = json!({"session_id": session, "title": title});
    let opened = dispatch(ctx, Profile::Full, "session.open", params).await?;
    Ok(opened["id"]
        .as_str()
        .context("session.open returned no id")?
        .to_owned())
}
