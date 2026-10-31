use super::*;

impl McpHost {
    /// Follow a section owner: `apply` reconciles to the section's latest value,
    /// whatever was published since.
    pub fn follow(&self, section: tokio::sync::watch::Receiver<Arc<McpSettings>>) {
        *self.inner.section.lock().unwrap() = Some(section);
    }

    /// Reconcile with the followed section (see `follow`); without one this
    /// leaves the running servers alone.
    pub async fn apply(&self, force: Option<&str>) -> Result<McpDiff> {
        let settings = self
            .inner
            .section
            .lock()
            .unwrap()
            .as_ref()
            .map(|section| section.borrow().clone());
        match settings {
            Some(settings) => self.reload(&settings, force).await,
            None => Ok(McpDiff::default()),
        }
    }
}
