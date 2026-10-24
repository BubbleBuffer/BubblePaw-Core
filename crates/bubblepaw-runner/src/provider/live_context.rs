//! Host attachment and quota bindings are independent of immutable generations.
use super::*;

impl LiveProviderConfig {
    /// Bind canonical attachments once; every generation and child binding uses it.
    pub fn with_image_store(mut self, store: Store) -> Self {
        self.snapshot().registry.set_usage_store(store.clone());
        self.image_store = Some(store);
        self
    }
    /// Share account quota observations and wait policy across every generation.
    pub fn with_quota_host(mut self, host: Arc<crate::quota::QuotaHost>) -> Self {
        self.snapshot().registry.set_quota_host(host.clone());
        self.quota_host = Some(host);
        self
    }
    pub(super) fn prepare_images(
        &self,
        generation: &ProviderGeneration,
        request: &mut ModelRequest,
    ) -> Result<()> {
        if let Some(store) = &self.image_store {
            crate::images::hydrate_request(
                request,
                store,
                generation.registry.capabilities(&request.model).image_input,
            )?;
        }
        Ok(())
    }
}
