//! Live settings classification, read from the section schema in `schema.rs`.
use super::*;

impl EffectiveConfig {
    /// Compare only settings that can be published without restarting the host.
    pub fn same_provider_config(&self, other: &Self) -> bool {
        !self.changes_from(other).iter().any(|change| change.live)
    }
    /// Detect edits to settings whose runtime resources remain fixed at startup.
    pub fn restart_needed(&self, startup: &Self) -> bool {
        !self.restart_only_keys(startup).is_empty()
    }
    /// The changed keys a running host cannot apply: the answer `config.set` will give.
    pub fn restart_only_keys(&self, startup: &Self) -> Vec<String> {
        self.changes_from(startup)
            .into_iter()
            .filter(|change| !change.live)
            .map(|change| change.path)
            .collect()
    }
}
