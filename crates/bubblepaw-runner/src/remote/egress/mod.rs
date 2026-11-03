//! Model traffic through a paired device.
//!
//! A host whose network blocks its model provider can send only its model
//! traffic through another paired device that is sharing. The agent loop,
//! tools, files and API key stay on the local host. Nothing here is remote
//! inference: the local model client opens its normal TLS connection to the
//! provider, but through an [`EgressProxy`] (a local HTTP CONNECT proxy) that
//! carries the connection's bytes through an end-to-end encrypted tunnel to the
//! forwarding device, which opens TCP to the provider and copies bytes. TLS
//! starts and ends on the local host, so the forwarding device never sees a
//! prompt, a reply or the key.
//!
//! The forwarding device only connects to public DNS names on port 443 that the
//! requesting side declared from its provider config ([`allow`]), and only for
//! devices the host user granted egress at pairing. See `docs/remote.md`.
pub mod allow;
mod forward;
mod proxy;
mod pump;
mod route;
mod wire;

pub use allow::{Refusal, check_host};
pub(crate) use forward::serve as serve_forward;
pub use forward::{EgressDial, SystemDial};
pub use proxy::{EgressHandle, EgressProxy};
pub use route::{EgressError, EgressErrorKind, EgressRoute};
