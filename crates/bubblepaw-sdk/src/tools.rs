//! Tool traits, the registry, and the runner's host capabilities.
//!
//! Model-facing tools are registered with [`ToolRegistry::register`]; host
//! capabilities go in with [`ToolRegistry::register_host`].

pub use bubblepaw_core::{
    JsonSchema, Tool, ToolCall, ToolContext, ToolDefinition, ToolDispatcher, ToolRegistry,
    ToolResult,
};
pub use bubblepaw_runner::{
    artifacts::ArtifactStore,
    codemode::CodeMode,
    register_native_tools_with_exec,
    runtime_note::RuntimeNotes,
    tools::{ProcessTools, WorkspaceTools, register_history_tools},
};
