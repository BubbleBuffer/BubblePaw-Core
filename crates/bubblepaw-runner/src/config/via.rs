//! `providers.<name>.via`: carry a provider's model traffic through a paired
//! device (`docs/remote.md`, "Model traffic through a paired device").
use super::*;
use crate::remote::RemotePaths;
use crate::remote::egress::{EgressHandle, EgressRoute};

impl ProviderSettings {
    /// `via` must be a saved host's device ID, on a provider kind that sends
    /// its own HTTPS requests, with an `https://` base URL naming a public host.
    pub(super) fn validate_via(&self, name: &str, kind: &str) -> Result<()> {
        let Some(via) = self.via.as_deref().filter(|via| !via.trim().is_empty()) else {
            return Ok(());
        };
        ensure!(
            bubblepaw_remote::device::DeviceId::parse(via).is_ok(),
            "Invalid config value: providers.{name}.via must be a saved host's device ID"
        );
        ensure!(
            matches!(
                kind,
                "codex" | "responses" | "chat-completions" | "anthropic-messages"
            ),
            "Invalid config value: providers.{name}.via applies to codex, responses, \
             chat-completions and anthropic-messages providers"
        );
        if let Some(base) = self
            .base_url
            .as_deref()
            .filter(|base| !base.trim().is_empty())
        {
            let paths = RemotePaths::in_data_dir(Path::new("."));
            EgressRoute::for_provider(&paths, via, base).map_err(|error| {
                anyhow::anyhow!("Invalid config value: providers.{name}.via: {error}")
            })?;
        }
        Ok(())
    }
}

impl Settings {
    /// The local proxy handle for `providers.<name>.via`, or `None` when the
    /// provider connects directly. The proxy starts when the first request needs
    /// it and stops with the last holder of the handle.
    pub fn egress_for(&self, name: &str) -> Result<Option<Arc<EgressHandle>>> {
        let Some(provider) = self.providers.get(name) else {
            return Ok(None);
        };
        let Some(via) = provider.via.as_deref().filter(|via| !via.trim().is_empty()) else {
            return Ok(None);
        };
        let base = provider
            .base_url
            .as_deref()
            .filter(|base| !base.trim().is_empty())
            .with_context(|| format!("providers.{name}.via needs providers.{name}.base_url"))?;
        let paths = RemotePaths::in_data_dir(&self.storage.data_dir);
        let route = EgressRoute::for_provider(&paths, via, base)
            .with_context(|| format!("providers.{name}.via"))?;
        Ok(Some(EgressHandle::new(route)))
    }
}
