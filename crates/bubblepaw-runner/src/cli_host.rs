//! Imports and helpers shared by the CLI request modules (`cli`, `host_attach`,
//! `host_lifetime`, `session_host`, `stdio_transport`). They live in the
//! library so any host process, the `bubblepaw` binary or an embedding app,
//! can execute an attached request.
pub(crate) use crate::{
    agent_handles::register_agent_handles,
    agents::AgentRegistry,
    artifacts::ArtifactStore,
    cli::{Cli, Command, TraceMode},
    cli_output::Output,
    codemode::CodeMode,
    config::{EffectiveConfig, Settings, selected_main_provider},
    episode_background::{EpisodeBackground, EpisodeTemplate},
    episode_models::{EpisodeModelConfig, HttpEpisodeModels},
    episode_worker::EpisodeWorker,
    host_attach::PreparedRequest,
    host_lifetime::HostLifetime,
    input_files::InputLimits,
    live_config::{LiveConfig, approval_policy},
    model_roles::{RoleDecisionProvider, approver_prompt},
    profiles::child_agent_host_live,
    provider::LiveProviderConfig,
    provider_setup::ProviderSetup,
    question::QuestionTools,
    register_native_tools_with_exec,
    root_prompt::root_system_prompt,
    runtime_note::RuntimeNotes,
    session_host,
    session_scope::SessionScope,
    sessions::title as session_title,
    summarization::{SummarizingContext, SummaryConfig},
    tools::register_history_tools,
    trace::{JsonlTrace, TraceConfig},
    usage::{UsageContext, UsageSettings},
    wake::WakeCoordinator,
};
pub(crate) use anyhow::{Context, Result, bail, ensure};
pub(crate) use bubblepaw_core::{
    AutoApprovalConfig, CancellationToken, ExecutionMode, Harness, HarnessConfig, HarnessEvent,
    Store, ToolRegistry,
};
pub(crate) use bubblepaw_mcp::McpHost;
pub(crate) use serde::{Deserialize, Serialize};
pub(crate) use serde_json::{Value, json};
pub(crate) use std::{path::PathBuf, sync::Arc, time::Duration};
pub(crate) use tokio::sync::mpsc;

pub fn episode_model_config(settings: &Settings) -> EpisodeModelConfig {
    EpisodeModelConfig {
        roles: settings.roles.clone(),
        retry_base_delay: Duration::from_millis(settings.retry_base_ms),
    }
}

pub fn validate_trace_path(
    path: &std::path::Path,
    data_dir: &std::path::Path,
    legacy: &std::path::Path,
) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;
    let data_dir = data_dir.canonicalize()?;
    let mut archives = vec![
        data_dir.join("catalog.db"),
        data_dir.join("serving.db"),
        data_dir.join("store.lock"),
    ];
    if legacy.exists() {
        archives.push(legacy.canonicalize()?);
    } else if let Some(parent) = legacy
        .parent()
        .filter(|p| !p.as_os_str().is_empty() && p.exists())
    {
        archives.push(
            parent.canonicalize()?.join(
                legacy
                    .file_name()
                    .context("Legacy database requires a file name")?,
            ),
        );
    } else {
        archives.push(std::env::current_dir()?.join(legacy));
    }
    let protected: Vec<_> = archives
        .iter()
        .flat_map(|archive| {
            let archive = archive.to_string_lossy();
            [
                archive.to_string(),
                format!("{archive}-wal"),
                format!("{archive}-shm"),
            ]
        })
        .collect();
    for suffix in ["", ".1", ".2"] {
        let mut name = path
            .file_name()
            .context("Trace path requires a file name")?
            .to_os_string();
        name.push(suffix);
        let candidate = parent.canonicalize()?.join(name);
        let candidate = if candidate.exists() {
            candidate.canonicalize()?
        } else {
            candidate
        };
        ensure!(
            !candidate.starts_with(data_dir.join("chats")),
            "Trace files and rotations must be separate from the archive and its SQLite sidecars"
        );
        let candidate = candidate.to_string_lossy();
        ensure!(
            !protected.iter().any(|p| if cfg!(windows) {
                candidate.eq_ignore_ascii_case(p)
            } else {
                candidate == p.as_str()
            }),
            "Trace files and rotations must be separate from the archive and its SQLite sidecars"
        );
    }
    Ok(())
}
