//! Small native tool pack. File paths are confined to the workspace; process
//! execution is a host capability, not a filesystem security sandbox.
mod history;
pub(crate) mod process;
mod workspace;
pub use history::register_history_tools;
pub use process::ProcessTools;
pub use workspace::WorkspaceTools;

use anyhow::Result;
use bubblepaw_core::ToolRegistry;
use std::{path::Path, sync::Arc};

/// The host retains this handle and calls shutdown before exiting. Read-only
/// registration exposes only listing, reading and searching; processes can write
/// arbitrary files and therefore are never classified as read-only.
/// Host tools whose full signatures the `js` description always shows; all
/// others are listed by name and discovered with `tools.$search`.
pub const ALWAYS_SHOWN_TOOLS: &[&str] = &[
    "read", "edit", "write", "search", "list", "exec", "poll", "wait", "cancel", "agent",
];

/// Registers the native tools and marks the runner's always-shown set.
pub fn register_native_tools(
    registry: &mut ToolRegistry,
    root: impl AsRef<Path>,
    read_only: bool,
) -> Result<Arc<ProcessTools>> {
    registry.show_always(ALWAYS_SHOWN_TOOLS.iter().copied());
    let workspace = WorkspaceTools::new(root)?;
    workspace.register(registry, read_only)?;
    let processes = Arc::new(ProcessTools::new(workspace));
    processes.register_wait(registry)?;
    if !read_only {
        processes.register(registry)?;
    }
    Ok(processes)
}
