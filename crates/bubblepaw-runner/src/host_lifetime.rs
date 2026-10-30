//! Lifetime of the shared runtime for one host process: one Harness and one quota host.
use crate::cli_host::*;
use bubblepaw_runner::quota::QuotaHost;
use std::sync::Mutex;

/// All harnesses share the Store runtime. Only the enclosing host may stop it.
/// The quota host is shared the same way, so its in-memory readings and change
/// tracking outlive a single request.
#[derive(Clone, Default)]
pub struct HostLifetime {
    harness: Arc<Mutex<Option<Arc<Harness>>>>,
    quota: Arc<Mutex<Option<Arc<QuotaHost>>>>,
    scope: SessionScope,
}
impl HostLifetime {
    pub fn new_scope(&self) -> SessionScope {
        self.scope.fork()
    }
    pub fn register(&self, harness: Arc<Harness>) {
        self.harness.lock().unwrap().get_or_insert(harness);
    }
    /// Builds the host on first use and returns the same Arc afterwards. Pollers
    /// come from the first request's settings; an account added later needs a
    /// host restart.
    pub fn quota_host(&self, build: impl FnOnce() -> Result<QuotaHost>) -> Result<Arc<QuotaHost>> {
        let mut slot = self.quota.lock().unwrap();
        if let Some(host) = slot.as_ref() {
            return Ok(host.clone());
        }
        let host = Arc::new(build()?);
        *slot = Some(host.clone());
        Ok(host)
    }
    /// The shared quota host, built from the first request's settings.
    pub fn quota_for(
        &self,
        store: &Store,
        settings: &Settings,
        effective: &EffectiveConfig,
    ) -> Result<Arc<QuotaHost>> {
        self.quota_host(|| {
            QuotaHost::from_settings_with_env(store.clone(), settings, &|name| {
                effective.environment(name)
            })
        })
    }
    pub async fn shutdown(&self) -> Result<()> {
        let harness = self.harness.lock().unwrap().take();
        if let Some(harness) = harness {
            harness.shutdown(Duration::from_secs(5)).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_of_one_lifetime_share_one_quota_host() -> Result<()> {
        let lifetime = HostLifetime::default();
        let clone = lifetime.clone();
        let builds = std::cell::Cell::new(0);
        let first = lifetime.quota_host(|| {
            builds.set(builds.get() + 1);
            QuotaHost::new(Store::in_memory()?, Duration::ZERO)
        })?;
        let second = clone.quota_host(|| {
            builds.set(builds.get() + 1);
            QuotaHost::new(Store::in_memory()?, Duration::ZERO)
        })?;
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(builds.get(), 1, "second request must reuse the first host");
        Ok(())
    }
}
