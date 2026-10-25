//! Web search and fetch settings shared by hosts.
//!
//! The whole `net` module of the runner is always compiled, so these types are
//! available without the `web` feature. The local web host itself is behind
//! [`crate::web`].

pub use bubblepaw_runner::net::{SearchBackend, WebConfig};
