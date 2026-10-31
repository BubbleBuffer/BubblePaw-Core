//! Optional provider and workspace adapters used by the thin BubblePaw runner.
//!
//! Register the host's native capabilities with [`register_native_tools_with_exec`]
//! and its JavaScript adapter with [`codemode::CodeMode::register`]. Configure
//! [`bubblepaw_core::HarnessConfig::execution_mode`] to choose JavaScript
//! composition explicitly. Native capabilities are the default model surface.
//!
//! Embedders can advertise exactly the harness's effective tool definitions with
//! [`bubblepaw_core::Harness::effective_tool_definitions`] and invoke one of them
//! without a model turn using [`bubblepaw_core::Harness::call_tool`]. Both APIs
//! preserve session restrictions and the normal dispatch/approval policy. A
//! single-tool call refuses an approval that needs an interactive decision.
// The CLI request modules still name the library by its crate path.
extern crate self as bubblepaw_runner;

pub mod agent_handles;
mod agent_limits;
pub mod agents;
pub mod artifacts;
#[cfg(feature = "bench")]
pub mod chapter_bench;
#[cfg(feature = "bench")]
#[doc(hidden)]
pub mod chapter_bench_cli;
mod child_workspaces;
#[doc(hidden)]
pub mod cli;
#[doc(hidden)]
pub mod cli_host;
pub mod cli_output;
pub mod codemode;
pub mod command_template;
pub mod config;
pub mod context_reads;
mod context_routes;
pub mod context_strategies;
pub mod delegate;
pub mod environments;
pub mod episode_actions;
pub mod episode_background;
pub mod episode_models;
#[cfg(feature = "bench")]
pub mod episode_prompt_bench;
pub mod episode_worker;
pub mod episodes;
mod exec_feed;
pub mod execution_mode;
mod history_snapshot;
pub mod host_api;
#[doc(hidden)]
pub mod host_attach;
pub mod host_client;
pub mod host_config;
#[doc(hidden)]
pub mod host_lifetime;
pub mod host_transport;
pub mod images;
pub mod input_files;
pub mod live_config;
pub mod logging;
pub mod memory_files;
pub mod model_roles;
pub mod net;
pub mod no_provider;
pub mod opencode_console;
pub mod pricing;
#[cfg(feature = "bench")]
pub mod probes;
mod process_output;
pub mod profiles;
pub mod provider;
pub mod provider_setup;
pub mod question;
pub mod quota;
pub mod quota_recovery;
pub mod quota_report;
pub mod quota_sources;
#[cfg(feature = "bench")]
pub mod replay;
pub mod research;
pub mod rewind;
pub mod root_prompt;
pub mod rpc_session;
pub mod runtime_note;
pub mod scratchpads;
pub mod session_control;
#[doc(hidden)]
pub mod session_host;
pub mod session_scope;
pub mod sessions;
pub mod settings_hub;
pub mod skills;
mod source_fingerprint;
pub mod spill;
mod stdio_transport;
pub mod summarization;
pub mod todo;
pub mod tools;
pub mod trace;
pub mod usage;
pub mod usage_report;
pub mod wake;
#[cfg(feature = "web")]
pub mod web;
#[cfg(feature = "web")]
pub mod web_providers;
pub mod worktrees;

/// Register the native pack with the runner's process settings and data directory.
pub fn register_native_tools_with_exec(
    registry: &mut bubblepaw_core::ToolRegistry,
    root: impl AsRef<std::path::Path>,
    read_only: bool,
    exec: config::ExecConfig,
    data_dir: impl AsRef<std::path::Path>,
) -> anyhow::Result<std::sync::Arc<tools::ProcessTools>> {
    registry.show_always(tools::ALWAYS_SHOWN_TOOLS.iter().copied());
    registry.show_always(["jobs"]);
    let spills = spill::SpillStore::new(data_dir.as_ref())?;
    spills.register(registry);
    let workspace =
        tools::WorkspaceTools::new(root)?.with_spill_data_dir(data_dir.as_ref().canonicalize()?);
    workspace.register(registry, read_only)?;
    let processes =
        std::sync::Arc::new(tools::ProcessTools::with_config(workspace, exec, data_dir)?);
    processes.register_wait(registry)?;
    if !read_only {
        processes.register(registry)?;
    }
    Ok(processes)
}
