//! Integration tests of one area (rewind), one binary: each former test file is a module here.

#[path = "../support/fixture_binary.rs"]
mod fixture_binary;
#[cfg(feature = "web")]
#[path = "../support/web_host.rs"]
mod web_host;
#[cfg(windows)]
#[path = "../support/windows_command.rs"]
mod windows_command;

mod replay;
mod rewind;
mod rewind_control_state;
mod rewind_transport;
mod runner;
