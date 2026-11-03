//! Sending a model adapter's requests through a paired device
//! (`providers.<name>.via`; `docs/remote.md`, "Model traffic through a paired
//! device"). The adapter's own TLS connection to the provider is unchanged: the
//! HTTP client only learns a local proxy, which tunnels the connection's bytes
//! to the forwarding device.
use super::{Client, builder};
use crate::provider::{
    ChatCompletionsProvider, MessagesProvider, ResponsesProvider, classify, transient,
};
use crate::remote::egress::EgressHandle;
use anyhow::{Result, anyhow};
use bubblepaw_core::ProviderErrorKind;
use std::sync::Arc;

/// Where a connection goes when the proxy could not be started: nowhere. Model
/// traffic for a `via` provider never falls back to a direct connection.
const CLOSED_PORT_PROXY: &str = "http://127.0.0.1:9";

impl Client {
    /// A client whose every connection goes through `egress`'s local proxy
    /// (and not through any environment proxy). The proxy starts on the first
    /// connection and stops when the last holder of the handle lets go.
    pub(in crate::provider) fn via(egress: Arc<EgressHandle>) -> Result<Self> {
        let proxy_for = egress.clone();
        let proxy = reqwest::Proxy::custom(move |_| {
            reqwest::Url::parse(&proxy_for.proxy_url())
                .or_else(|_| reqwest::Url::parse(CLOSED_PORT_PROXY))
                .ok()
        });
        Ok(Self {
            inner: builder().no_proxy().proxy(proxy).build()?,
            egress: Some(egress),
        })
    }

    /// The error for a request that could not be sent: `generic`, plus the
    /// reason the paired device gave when the request went through one and it
    /// is offline or refused. A refusal is fatal (nothing retries it away); an
    /// offline device is transient.
    pub(in crate::provider) fn transport_failed(&self, generic: String) -> anyhow::Error {
        match self.egress.as_ref().and_then(|egress| egress.failure()) {
            Some(failure) => classify(
                if failure.is_permanent() {
                    ProviderErrorKind::Fatal
                } else {
                    ProviderErrorKind::Transient
                },
                anyhow!("{generic}: {failure}"),
            ),
            None => transient(anyhow!(generic)),
        }
    }
}

impl ChatCompletionsProvider {
    /// Send this provider's requests through the paired device `egress` names.
    pub fn with_egress(mut self, egress: Arc<EgressHandle>) -> Result<Self> {
        self.client = Client::via(egress)?;
        Ok(self)
    }
}

impl MessagesProvider {
    /// Send this provider's requests through the paired device `egress` names.
    pub fn with_egress(mut self, egress: Arc<EgressHandle>) -> Result<Self> {
        self.client = Client::via(egress)?;
        Ok(self)
    }
}

impl ResponsesProvider {
    /// Send this provider's requests through the paired device `egress` names.
    pub fn with_egress(mut self, egress: Arc<EgressHandle>) -> Result<Self> {
        self.client = Client::via(egress)?;
        Ok(self)
    }
}
