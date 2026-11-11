//! BubblePaw's stable surface for embedders.
//!
//! One dependency is enough: this crate re-exports the curated core and runner
//! types an outside program needs, and adds [`HostBuilder`] for the wiring an
//! embedder writes by hand. The engine behind the facade is [`bubblepaw_core`];
//! the provider adapters, workspace tools and context strategies come from
//! [`bubblepaw_runner`].
//!
//! # Stability
//!
//! Everything reachable through the named modules ([`store`], [`harness`],
//! [`events`], [`approvals`], [`tools`], [`providers`], [`context`],
//! [`todo`], [`questions`], [`net`], [`web`], [`claude_code`], [`provider_keys`],
//! [`sessions`], [`tunnel_settings`]), plus [`Host`], [`HostBuilder`]
//! and [`Client`] with [`ClientError`] and [`Notifications`] (and, from
//! 0.5.0, [`HostConfig`] with [`ConfigOptions`] and [`ConfigError`]),
//! follows semantic versioning from 0.2.0: those paths keep working within a
//! major release, and a breaking change is a version bump.
//!
//! The whole-crate re-exports [`core`] and [`runner`] are an escape hatch with
//! **no stability promise**. They exist so an embedder is never stuck; anything
//! reached through them may change in a patch release. Prefer the named modules.
//!
//! # Example
//!
//! The runnable example is `examples/scripted_chat.rs`; it wires a scripted
//! provider, one host tool and a harness, then prints the final text:
//!
//! ```rust,ignore
//! let host = HostBuilder::new(store, providers)
//!     .workspace(workspace_dir)
//!     .tools(registry)
//!     .history_tools(true)
//!     .todo(true)
//!     .code_mode(artifacts_dir)
//!     .summarize(SummaryConfig::default())
//!     .config(HarnessConfig { model: "scripted".into(), ..Default::default() })
//!     .build()?;
//! dispatch(host.api(), Profile::Full, "run.start",
//!     serde_json::json!({"session_id":session.id,"text":"Hello"})).await?;
//! let run = dispatch(host.api(), Profile::Full, "run.wait",
//!     serde_json::json!({"session_id":session.id})).await?;
//! ```

#[cfg(feature = "host")]
pub mod approvals;
#[cfg(feature = "host")]
pub mod claude_code;
mod client;
#[cfg(feature = "host")]
pub mod context;
#[cfg(feature = "host")]
pub mod events;
#[cfg(feature = "host")]
pub mod harness;
#[cfg(feature = "host")]
mod host;
#[cfg(feature = "host")]
pub mod net;
pub mod provider_keys;
#[cfg(feature = "host")]
pub mod providers;
#[cfg(feature = "host")]
pub mod questions;
pub mod remote;
pub mod sessions;
#[cfg(feature = "host")]
pub mod store;
#[cfg(feature = "host")]
pub mod todo;
#[cfg(feature = "host")]
pub mod tools;
pub mod tunnel_settings;
#[cfg(feature = "web")]
pub mod web;

/// The engine, as a whole crate. **No stability promise**; see the crate docs.
pub use bubblepaw_core as core;
/// The adapters and tools, as a whole crate. **No stability promise**; see the
/// crate docs.
pub use bubblepaw_runner as runner;

/// The same typed host method table used by stdio, HTTP and the CLI.
#[cfg(feature = "host")]
pub use bubblepaw_runner::host_api;
#[cfg(feature = "host")]
pub use bubblepaw_runner::host_api::{HostCtx, HostError, Method, ReadScope, dispatch};
/// The host-API profile a device holds.
pub use bubblepaw_runner::profile::Profile;

#[cfg(feature = "host")]
/// The CLI host's configuration, for [`Client::open_configured`].
pub use bubblepaw_runner::host_config::{ConfigError, ConfigOptions, HostConfig};
#[cfg(feature = "host")]
/// The user-level record of trusted workspaces, for [`ConfigOptions::trust_workspace`] and embedders that grant trust.
pub use bubblepaw_runner::workspace_trust;
pub use client::{Client, ClientError, Notifications};
#[cfg(feature = "host")]
pub use host::{Host, HostBuilder};

#[cfg(feature = "host")]
/// The items nearly every embedder needs.
pub mod prelude {
    pub use crate::approvals::{Approval, ApprovalPolicy};
    pub use crate::client::{Client, ClientError};
    pub use crate::context::SummarizingContext;
    pub use crate::harness::{
        CancellationToken, ExecutionMode, Harness, HarnessConfig, RunOptions,
    };
    pub use crate::host::{Host, HostBuilder};
    pub use crate::host_api::{HostCtx, HostError, Method, Profile, ReadScope, dispatch};
    pub use crate::providers::{Provider, ProviderRegistry, ScriptedProvider, ScriptedStep};
    pub use crate::store::{Message, Role, Session, Store};
    pub use crate::tools::{Tool, ToolDefinition, ToolRegistry, ToolResult};
}
