//! Rebuilding an accepted provider generation for a credential change.
use super::*;

impl LiveProviderConfig {
    pub fn prepare(&self, effective: EffectiveConfig) -> Result<PreparedProviderConfig> {
        self.prepare_inner(effective, false)
    }
    /// Removing the main route's last key is deliberate. In-flight requests
    /// retain the previous generation; the next request sees the new key state.
    pub(crate) fn prepare_credentials(&self) -> Result<PreparedProviderConfig> {
        self.prepare_inner(self.snapshot().effective.clone(), true)
    }
    pub fn reload(&self) -> Result<Arc<ProviderGeneration>> {
        let _update = self.lock_update()?;
        let effective = self.startup.reload()?;
        let candidate = self.prepare(effective)?;
        Ok(self.apply_prepared(candidate))
    }
    pub fn main_model(&self) -> String {
        self.snapshot().settings.main_model.clone()
    }
}
