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

#[cfg(feature = "host")]
pub mod agent_handles;
#[cfg(feature = "host")]
mod agent_limits;
#[cfg(feature = "host")]
pub mod agents;
#[cfg(feature = "host")]
pub mod artifacts;
#[cfg(feature = "bench")]
#[cfg(feature = "host")]
pub mod chapter_bench;
#[cfg(feature = "bench")]
#[doc(hidden)]
#[cfg(feature = "host")]
pub mod chapter_bench_cli;
#[cfg(feature = "host")]
mod child_workspaces;
#[doc(hidden)]
#[cfg(feature = "host")]
pub mod cli;
#[doc(hidden)]
#[cfg(feature = "host")]
pub mod cli_host;
#[cfg(feature = "host")]
pub mod cli_output;
#[cfg(feature = "host")]
pub mod codemode;
#[cfg(feature = "host")]
pub mod command_template;
#[cfg(feature = "host")]
pub mod config;
#[cfg(feature = "host")]
pub mod config_set;
#[cfg(feature = "host")]
pub mod context_reads;
#[cfg(feature = "host")]
mod context_routes;
#[cfg(feature = "host")]
pub mod context_strategies;
#[cfg(feature = "host")]
pub mod delegate;
#[cfg(feature = "host")]
pub mod environments;
#[cfg(feature = "host")]
pub mod episode_actions;
#[cfg(feature = "host")]
pub mod episode_background;
#[cfg(feature = "host")]
pub mod episode_models;
#[cfg(feature = "bench")]
#[cfg(feature = "host")]
pub mod episode_prompt_bench;
#[cfg(feature = "host")]
pub mod episode_worker;
#[cfg(feature = "host")]
pub mod episodes;
#[cfg(feature = "host")]
mod exec_feed;
#[cfg(feature = "host")]
pub mod execution_mode;
#[cfg(feature = "host")]
mod history_snapshot;
#[cfg(feature = "host")]
pub mod host_api;
#[doc(hidden)]
#[cfg(feature = "host")]
pub mod host_attach;
pub mod host_client;
#[cfg(feature = "host")]
pub mod host_config;
#[doc(hidden)]
#[cfg(feature = "host")]
pub mod host_lifetime;
pub mod host_transport;
#[cfg(feature = "host")]
pub mod images;
#[cfg(feature = "host")]
pub mod input_files;
#[cfg(feature = "host")]
pub mod live_config;
#[cfg(feature = "host")]
pub mod logging;
#[cfg(feature = "host")]
pub mod memory_files;
#[cfg(feature = "host")]
pub mod model_roles;
#[cfg(feature = "host")]
pub mod net;
pub mod no_provider;
#[cfg(feature = "host")]
pub mod opencode_console;
#[cfg(feature = "host")]
pub mod pricing;
#[cfg(feature = "bench")]
#[cfg(feature = "host")]
pub mod probes;
#[cfg(feature = "host")]
mod process_output;
pub mod profile;
#[cfg(feature = "host")]
pub mod profiles;
#[cfg(feature = "host")]
pub mod provider;
#[cfg(feature = "host")]
mod provider_keys;
#[cfg(feature = "host")]
pub mod provider_setup;
#[cfg(feature = "host")]
pub mod question;
#[cfg(feature = "host")]
pub mod quota;
#[cfg(feature = "host")]
pub mod quota_recovery;
#[cfg(feature = "host")]
pub mod quota_report;
#[cfg(feature = "host")]
pub mod quota_sources;
pub mod remote;
#[cfg(feature = "bench")]
#[cfg(feature = "host")]
pub mod replay;
#[cfg(feature = "host")]
pub mod research;
#[cfg(feature = "host")]
pub mod rewind;
#[cfg(feature = "host")]
pub mod root_prompt;
pub mod rpc_session;
#[cfg(feature = "host")]
pub mod runtime_note;
#[cfg(feature = "host")]
pub mod scratchpads;
#[cfg(feature = "host")]
pub mod session_control;
#[doc(hidden)]
#[cfg(feature = "host")]
pub mod session_host;
#[cfg(feature = "host")]
pub mod session_scope;
#[cfg(feature = "host")]
pub mod sessions;
#[cfg(feature = "host")]
pub mod settings_hub;
#[cfg(feature = "host")]
pub mod skills;
#[cfg(feature = "host")]
mod source_fingerprint;
#[cfg(feature = "host")]
pub mod spill;
#[cfg(feature = "host")]
mod stdio_transport;
#[cfg(feature = "host")]
pub mod summarization;
#[cfg(feature = "host")]
pub mod todo;
#[cfg(feature = "host")]
pub mod tools;
#[cfg(feature = "host")]
pub mod toolsets;
#[cfg(feature = "host")]
pub mod trace;
#[cfg(feature = "host")]
pub mod trust_cli;
#[cfg(feature = "host")]
pub mod usage;
#[cfg(feature = "host")]
pub mod usage_report;
#[cfg(feature = "host")]
pub mod wake;
#[cfg(feature = "web")]
#[cfg(feature = "host")]
pub mod web;
#[cfg(feature = "web")]
#[cfg(feature = "host")]
pub mod web_providers;
#[cfg(feature = "host")]
pub mod workspace_trust;
#[cfg(feature = "host")]
pub mod worktrees;

#[cfg(feature = "host")]
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
