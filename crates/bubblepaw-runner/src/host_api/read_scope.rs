use super::*;
use std::path::Path;

/// Data a host may read, independent of its mutation workspace and run ownership.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadScope {
    Host,
    Workspace(PathBuf),
}
impl ReadScope {
    /// Metadata only: checking a binding neither restores an archive nor binds
    /// a legacy chat. Unbound legacy sessions keep their existing accessibility.
    pub fn allows(&self, store: &Store, id: &str) -> Result<bool, HostError> {
        match self {
            Self::Host => Ok(true),
            Self::Workspace(workspace) => Ok(store
                .session_workspace(id)?
                .is_none_or(|bound| bound == canonical(workspace))),
        }
    }
    pub fn ensure(&self, store: &Store, id: &str) -> Result<(), HostError> {
        store.session_root_id(id)?;
        if let Self::Workspace(workspace) = self
            && let Some(bound) = store.session_workspace(id)?
            && bound != canonical(workspace)
        {
            return Err(
                anyhow::Error::new(crate::session_control::WorkspaceMismatch(bound)).into(),
            );
        }
        Ok(())
    }
}
fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_owned())
}
