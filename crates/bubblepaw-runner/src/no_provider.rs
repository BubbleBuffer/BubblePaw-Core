//! The typed "nothing to call a model with" state. A host starts without a
//! usable provider so a client can configure one through the host API;
//! operations that need a model fail with [`NoProviderConfigured`], which the
//! host API reports as [`CODE`].
use std::fmt;

/// The JSON-RPC error code of [`NoProviderConfigured`] on every host-API
/// adapter (HTTP status 409).
pub const CODE: i32 = -32004;

/// The main model's route has no provider, or the provider has no usable
/// credentials. The message names the config keys and host methods that fix it
/// and never contains command-line flags, so it is safe to show to any client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoProviderConfigured {
    /// The provider route the main model resolves to.
    pub route: String,
    /// What is missing, in config-key terms.
    pub reason: String,
}

impl NoProviderConfigured {
    pub fn new(route: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            route: route.into(),
            reason: reason.into(),
        }
    }
}

impl fmt::Display for NoProviderConfigured {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "no model provider is configured for the main model route {}: {}. Set model.provider \
             and providers.<id> in the config, or configure one with the providers.save and \
             roles.set_main host methods",
            self.route, self.reason
        )
    }
}

impl std::error::Error for NoProviderConfigured {}
