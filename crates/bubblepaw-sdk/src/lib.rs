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
//! [`todo`], [`questions`], [`net`], [`web`], [`claude_code`]), plus [`Host`], [`HostBuilder`]
//! and [`Client`] with [`ClientError`] and [`Notifications`],
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

pub mod approvals;
pub mod claude_code;
mod client;
pub mod context;
pub mod events;
pub mod harness;
mod host;
pub mod net;
pub mod providers;
pub mod questions;
pub mod store;
pub mod todo;
pub mod tools;
#[cfg(feature = "web")]
pub mod web;

/// The engine, as a whole crate. **No stability promise**; see the crate docs.
pub use bubblepaw_core as core;
/// The adapters and tools, as a whole crate. **No stability promise**; see the
/// crate docs.
pub use bubblepaw_runner as runner;

/// The same typed host method table used by stdio, HTTP and the CLI.
pub use bubblepaw_runner::host_api;
pub use host_api::{HostCtx, HostError, Method, Profile, ReadScope, dispatch};

pub use client::{Client, ClientError, Notifications};
pub use host::{Host, HostBuilder};

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
