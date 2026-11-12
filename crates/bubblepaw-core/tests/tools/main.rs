//! Integration tests of one area (tools, controls and results), one binary: each former test file is a module here.

#[path = "../support/mod.rs"]
mod support;

mod hardline_workspace;
mod native_tool_snapshot;
mod provider_binding;
mod run_controls;
mod structured_output;
mod submit_result;
mod tool_context_approval;
mod tool_discovery;
mod tool_restriction;
mod tools_off;
