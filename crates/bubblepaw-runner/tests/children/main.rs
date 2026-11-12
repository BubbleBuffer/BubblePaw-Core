//! Integration tests of one area (children), one binary: each former test file is a module here.

#[path = "../support/eventually.rs"]
mod eventually;
#[path = "../support/fixture_binary.rs"]
mod fixture_binary;
#[path = "../support/unmarked.rs"]
mod unmarked;
#[cfg(windows)]
#[path = "../support/windows_command.rs"]
mod windows_command;

mod agent_rejections;
mod agents;
mod agents_repl;
mod approvals;
mod auto_approvals_cli;
mod auto_wake;
mod child_context;
mod child_controls;
mod child_controls_js;
mod child_sessions;
mod delegate;
mod notice_delivery_metadata;
mod question_routing;
mod questions;
mod recursive_children;
mod research;
mod research_live400;
mod round_limits;
mod submit_result;
mod worker_lifecycle;
mod worktree_children;
