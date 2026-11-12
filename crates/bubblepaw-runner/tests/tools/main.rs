//! Integration tests of one area (tools), one binary: each former test file is a module here.

#[path = "../support/checkpoint_write.rs"]
mod checkpoint_write;
#[path = "../support/fixture_binary.rs"]
mod fixture_binary;
#[path = "../support/notice_fixture.rs"]
mod notice_fixture;
#[path = "../support/notice_provider.rs"]
mod notice_provider;
#[cfg(windows)]
#[path = "../support/windows_command.rs"]
mod windows_command;

mod artifacts;
mod builtin_tool_schemas;
mod codemode_runtime;
mod command_template;
mod exec_codemode;
mod exec_jobs;
mod exec_stdio;
mod execution_mode;
mod execution_mode_resume;
mod history_tools;
mod images;
mod input_cli;
mod input_files;
mod mcp;
mod native_batch_exec;
mod native_batching;
mod native_effects;
mod native_exec;
mod process_workspaces;
mod scratchpad_exec;
mod scratchpad_files;
mod scratchpad_prompt;
mod scratchpads;
mod script_runs;
mod skills;
mod tool_defaults;
mod tool_search;
mod tools_native;
mod workspace_edit;
