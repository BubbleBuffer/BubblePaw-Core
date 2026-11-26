use super::*;

impl ProcessTools {
    pub(super) fn workdir(&self, context: &ToolContext, path: &str) -> Result<PathBuf> {
        self.workspace.retain_scratch_workspace()?;
        if let Some(store) = self.store.get() {
            let scratchpads = self.workspace.scratchpads(store, &context.session_id)?;
            if let Some(path) = scratchpads.resolve_from(self.workspace.root(), path, false)? {
                return Ok(path);
            }
        }
        self.workspace.existing(path)
    }
    /// Register `exec`, `poll`, `cancel` and `jobs` with no store of their own, for
    /// fixtures that call them with sessions no store knows. Without a store a
    /// command's working directory does not resolve scratchpads and a finished
    /// job's completion notice is not persisted. Hosts use [`ProcessTools::register`].
    #[doc(hidden)]
    pub fn register_without_store(self: &Arc<Self>, registry: &mut ToolRegistry) -> Result<()> {
        for name in ["exec", "poll", "cancel", "jobs"] {
            registry.register_host(Arc::new(ProcessTool {
                processes: self.clone(),
                name,
            }))?;
        }
        Ok(())
    }
}
