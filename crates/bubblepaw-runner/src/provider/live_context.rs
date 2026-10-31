//! Host attachment and quota bindings are independent of immutable generations.
use super::*;

impl ProviderGeneration {
    /// Explicit context capacity wins; otherwise use Codex metadata for the
    /// requested route's model, including after a main-model reload.
    pub fn context_window(&self, model: &str) -> Option<usize> {
        self.settings
            .context_window_tokens
            .or_else(|| self.catalog_window(model))
    }
    /// Codex catalog capacity for the route's model; the configured window is
    /// the context section's and is not read here.
    pub fn catalog_window(&self, model: &str) -> Option<usize> {
        let (provider, _, model) = self.registry.route(model);
        if self
            .settings
            .providers
            .get(&provider)
            .is_some_and(|settings| settings.kind.as_deref() == Some("codex"))
        {
            self.effective
                .codex_models_cache()
                .as_ref()
                .and_then(|path| crate::summarization::cached_context_window(path, &model))
        } else {
            None
        }
    }
}

impl LiveProviderConfig {
    /// Subscribe to the model/provider section; each value is a whole generation.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<Arc<ProviderGeneration>> {
        self.active.subscribe()
    }
    /// The context and MCP section owners, published together with each generation.
    pub fn hub(&self) -> &crate::settings_hub::SettingsHub {
        &self.hub
    }
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
