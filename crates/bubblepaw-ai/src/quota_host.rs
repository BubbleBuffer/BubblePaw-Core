//! What a provider call needs to know about the quota host driving it.
use crate::ProviderError;
use std::time::Duration;

/// Longest wait a retry or the quota host takes inline.
pub const SHORT_WAIT: Duration = Duration::from_secs(15);

/// Stops outer retry/fallback loops after the quota policy has made a terminal
/// decision. The typed provider error remains discoverable through its source.
#[derive(Debug)]
pub struct QuotaStop(pub ProviderError);
impl std::fmt::Display for QuotaStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for QuotaStop {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

pub fn is_quota_stop(error: &anyhow::Error) -> bool {
    error.downcast_ref::<QuotaStop>().is_some()
}
