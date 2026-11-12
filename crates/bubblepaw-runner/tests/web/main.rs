//! Integration tests of one area (web), one binary: each former test file is a module here.

#[path = "../support/fixture_binary.rs"]
mod fixture_binary;
#[path = "../support/http_server.rs"]
mod http_server;
#[cfg(feature = "web")]
#[path = "../support/web_host.rs"]
mod web_host;
#[cfg(feature = "web")]
#[cfg(windows)]
#[path = "../support/windows_command.rs"]
mod windows_command;

mod control_stdio;
mod control_web;
mod host_api_guard;
mod host_api_parity;
mod host_rpc;
mod session_control;
mod session_lifecycle;
mod stdio_host_parity;
mod web_child_question;
mod web_cli;
mod web_codemode;
mod web_fallback;
mod web_input_files;
mod web_live;
mod web_processes;
mod web_providers;
mod web_transport;
mod web_wake;
mod web_workers;
