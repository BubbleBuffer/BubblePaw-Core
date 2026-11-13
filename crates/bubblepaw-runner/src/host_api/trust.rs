//! Workspace trust for hosts: list, grant and revoke the user-level trust records, and the
//! `workspace_trust` host-state event a new session carries while its workspace's project
//! config is skipped. Local callers only: a paired remote device never decides trust.
use super::*;
use crate::{
    live_config::ReloadReport,
    workspace_trust::{self, TrustRecord, TrustStatus},
};

/// Host-state key (and event key) holding the skipped-config notice.
pub const STATE_KEY: &str = "workspace_trust";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathParams {
    pub path: PathBuf,
}

#[derive(Serialize)]
pub struct ListResult {
    pub workspaces: Vec<TrustRecord>,
    /// How this host treated its own workspace's project config; null without a live config.
    pub workspace: Option<Value>,
}

#[derive(Serialize)]
pub struct TrustResult {
    pub trusted: TrustRecord,
    /// The reload that applies the change; null when the host has no live config.
    pub reload: Option<ReloadReport>,
}

#[derive(Serialize)]
pub struct RevokeResult {
    pub removed: bool,
    pub reload: Option<ReloadReport>,
}

fn home(ctx: &HostCtx) -> Result<PathBuf, HostError> {
    ctx.active_providers()
        .and_then(|active| active.snapshot().effective.load_options().home)
        .or_else(workspace_trust::default_home)
        .ok_or_else(|| HostError::unavailable("Workspace trust needs a home directory"))
}

fn root(path: &std::path::Path) -> Result<PathBuf, HostError> {
    if !path.is_dir() {
        return Err(HostError::invalid(format!(
            "{} is not a directory",
            path.display()
        )));
    }
    Ok(workspace_trust::canonical(path))
}

async fn reloaded(ctx: &HostCtx) -> Option<ReloadReport> {
    match &ctx.live_config {
        Some(live) => Some(live.reload(None).await),
        None => None,
    }
}

/// While the host's workspace has a project config that is skipped for want of trust, record
/// that on a new root session so a client sees it on open and through `events.after`.
pub(super) fn note_untrusted(ctx: &HostCtx, session_id: &str) {
    let Some(active) = ctx.active_providers() else {
        return;
    };
    let state = active.snapshot().effective.workspace_trust();
    if state.status != TrustStatus::Untrusted {
        return;
    }
    if let Err(error) = ctx
        .store
        .update_host_state(session_id, "", STATE_KEY, |_| Ok(json!(state)))
    {
        tracing::warn!(%error, "cannot record the skipped workspace config on the session");
    }
}

pub(super) async fn command(
    ctx: &HostCtx,
    method: MethodId,
    params: Value,
) -> Result<Value, HostError> {
    if ctx.remote_caller {
        return Err(HostError::denied(&format!(
            "{} (local callers only)",
            method.name()
        )));
    }
    match method {
        MethodId::WorkspaceTrustList => {
            let _: EmptyParams = decode(params)?;
            let workspaces = workspace_trust::list(&home(ctx)?)
                .map_err(|error| HostError::internal(error.to_string()))?;
            let workspace = ctx
                .active_providers()
                .map(|active| json!(active.snapshot().effective.workspace_trust()));
            encode(ListResult {
                workspaces,
                workspace,
            })
        }
        MethodId::WorkspaceTrustTrust => {
            let input: PathParams = decode(params)?;
            let trusted = workspace_trust::trust(&home(ctx)?, &root(&input.path)?)
                .map_err(|error| HostError::internal(error.to_string()))?;
            encode(TrustResult {
                trusted,
                reload: reloaded(ctx).await,
            })
        }
        MethodId::WorkspaceTrustRevoke => {
            let input: PathParams = decode(params)?;
            let removed = workspace_trust::untrust(&home(ctx)?, &root(&input.path)?)
                .map_err(|error| HostError::internal(error.to_string()))?;
            encode(RevokeResult {
                removed,
                reload: reloaded(ctx).await,
            })
        }
        _ => unreachable!("workspace trust method table"),
    }
}
