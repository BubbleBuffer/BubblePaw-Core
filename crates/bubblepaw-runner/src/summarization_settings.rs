//! Replaceable host settings and apply-time prompt refresh.
use super::*;

/// Prepare a new host-owned prompt component for an applied compaction. The
/// returned metadata is published atomically with that view, never on reuse.
pub type SurfaceRefresh =
    Arc<dyn Fn(&str, &str) -> Result<Option<bubblepaw_core::ContextSurfaceRebind>> + Send + Sync>;

impl SummarizingContext {
    /// Record that this context deliberately changed the session's serving prefix.
    pub(super) fn mark_prefix_change(&self, session: &str, reason: &str) {
        self.prefix_changes
            .lock()
            .unwrap()
            .insert(session.to_owned(), reason.to_owned());
    }
    /// Explicit compaction remains available when automatic work is disabled.
    pub fn with_auto_summarize(mut self, enabled: bool) -> Self {
        self.automatic = enabled;
        self
    }
    pub fn with_surface_refresh(mut self, refresh: SurfaceRefresh) -> Self {
        self.surface_refresh = Some(refresh);
        self
    }

    pub(super) fn refresh_surface(
        &self,
        session: &str,
        request: &mut ModelRequest,
    ) -> Result<Option<bubblepaw_core::ContextSurfaceRebind>> {
        let update = self
            .surface_refresh
            .as_ref()
            .map(|refresh| refresh(session, &request.system_prompt))
            .transpose()?
            .flatten();
        if let Some(update) = &update {
            request.system_prompt = update.system_prompt.clone();
        }
        Ok(update)
    }
    /// Only capacity, compaction threshold and automatic preparation are live.
    /// LCM settings, strategy and all other context resources stay fixed.
    pub fn with_live_config(mut self, live: Arc<crate::provider::LiveProviderConfig>) -> Self {
        self.context = Some(live.hub().context());
        self.live = Some(live);
        self
    }
    /// Capacity, threshold and automatic preparation come from the context section.
    pub(super) fn apply_live_context(
        &self,
        config: &mut SummaryConfig,
        live: &crate::provider::LiveProviderConfig,
        model: &str,
    ) {
        if let Some(context) = self.context.as_ref().map(|rx| rx.borrow().clone()) {
            config.threshold_cap = context.compact_threshold_tokens;
            config.context_window = context
                .window_tokens
                .or_else(|| live.snapshot().catalog_window(model));
        }
    }
    pub(super) fn automatic(&self) -> bool {
        self.context
            .as_ref()
            .map_or(self.automatic, |context| context.borrow().auto_summarize)
    }
}
