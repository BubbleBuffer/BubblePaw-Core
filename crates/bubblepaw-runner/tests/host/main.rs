//! Integration tests of one area (host), one binary: each former test file is a module here.

#[path = "../support/episode_fixture.rs"]
mod episode_fixture;
#[path = "../support/message_fixture.rs"]
mod message_fixture;
#[path = "../support/notice_fixture.rs"]
mod notice_fixture;
#[cfg(unix)]
#[path = "../support/proc_sockets.rs"]
mod proc_sockets;
#[cfg(feature = "web")]
#[path = "../support/web_host.rs"]
mod web_host;

mod central_store;
mod cold_tier;
mod config_cli;
mod config_runtime;
mod extension_storage;
mod host_attach;
mod host_attach_mcp;
mod host_config;
mod host_provider_reload;
mod live_agent_settings;
mod live_config;
mod live_config_routes;
mod live_config_tool_generation;
mod memory_files;
mod no_provider_cli;
mod runtime_note;
mod storage_cli;
mod trace_cli;
mod tracing;
mod trust_api;
mod wake_storage;
