use super::*;
use crate::scratchpads::{ScratchpadStore, Scratchpads};
use bubblepaw_core::Store;

#[derive(Clone, Default)]
pub(super) struct ScratchAccess {
    configured: Arc<RwLock<Option<ScratchpadStore>>>,
    stores: Arc<Mutex<BTreeMap<PathBuf, ScratchpadStore>>>,
    temporary: Arc<Mutex<Option<ScratchpadStore>>>,
}

impl WorkspaceTools {
    /// Acquire the workspace's live host lease before running retention. Child
    /// workspace views share this service and keep their own leases alive too.
    pub fn prepare_scratchpads(&self, data_dir: impl AsRef<Path>) -> Result<ScratchpadStore> {
        fs::create_dir_all(data_dir.as_ref())?;
        let data_dir = data_dir.as_ref().canonicalize()?;
        let service = self.scratch_store_at(&data_dir)?;
        service.lease_workspace(&self.root)?;
        *self
            .scratch
            .configured
            .write()
            .map_err(|_| anyhow!("scratch service poisoned"))? = Some(service.clone());
        Ok(service)
    }

    /// Resolve paths for a model prompt or an ordinary dispatcher call. Explicit
    /// host data configuration wins even when the dispatcher uses an in-memory
    /// Store. Only hosts with neither source use an owned temporary directory.
    pub fn scratchpads(&self, store: &Store, session: &str) -> Result<Scratchpads> {
        self.scratch_service(store.data_dir())?
            .for_session(store, session, &self.root)
    }

    fn scratch_store_at(&self, data: &Path) -> Result<ScratchpadStore> {
        let mut stores = self
            .scratch
            .stores
            .lock()
            .map_err(|_| anyhow!("scratch services poisoned"))?;
        if let Some(service) = stores.get(data) {
            return Ok(service.clone());
        }
        let service = ScratchpadStore::new(data)?;
        stores.insert(data.to_owned(), service.clone());
        Ok(service)
    }

    fn scratch_service(&self, data: Option<&Path>) -> Result<ScratchpadStore> {
        if let Some(configured) = self
            .scratch
            .configured
            .read()
            .map_err(|_| anyhow!("scratch service poisoned"))?
            .as_ref()
        {
            return Ok(configured.clone());
        }
        if let Some(data) = self.spill_data_dir.as_deref().or(data) {
            return self.scratch_store_at(data);
        }
        let mut temporary = self
            .scratch
            .temporary
            .lock()
            .map_err(|_| anyhow!("scratch service poisoned"))?;
        if temporary.is_none() {
            *temporary = Some(ScratchpadStore::temporary()?);
        }
        Ok(temporary.as_ref().unwrap().clone())
    }

    /// Infallible process views inherit the host's scratch identity immediately;
    /// their first command acquires the new workspace lease with ordinary error
    /// propagation. The production child factory acquires it during binding.
    pub(crate) fn inherit_host_scratch(&mut self, host: &WorkspaceTools) {
        self.scratch = host.scratch.clone();
        self.spill_data_dir = host.spill_data_dir.clone();
    }

    pub(crate) fn retain_scratch_workspace(&self) -> Result<()> {
        if let Some(service) = self
            .scratch
            .configured
            .read()
            .map_err(|_| anyhow!("scratch service poisoned"))?
            .as_ref()
        {
            service.lease_workspace(&self.root)?;
        }
        Ok(())
    }

    pub(super) fn scratch_mutation(
        &self,
        scratchpads: Scratchpads,
        resolved: &Path,
    ) -> Result<Self> {
        let root = scratchpads
            .root_for(resolved)
            .context("path escapes scratchpads")?;
        let mut workspace = Self::new(root)?;
        workspace.scratch = self.scratch.clone();
        workspace.spill_data_dir = self.spill_data_dir.clone();
        workspace.scratch_boundary = Some(scratchpads);
        Ok(workspace)
    }
}
