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
}
