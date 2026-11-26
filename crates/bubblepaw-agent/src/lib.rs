//! The in-memory run loop and tool dispatcher of BubblePaw.
//!
//! This crate runs one run in memory: the loop that asks a model for a reply and
//! dispatches its tool calls, the tool registry and dispatcher, and the plain data
//! and traits between them. It knows no database, HTTP client or session shell; the
//! host supplies those through the [`run_journal::RunJournal`],
//! [`request_seam::RequestContext`], [`tool_source::ToolSource`],
//! [`tool_gate::ToolGate`] and [`live_events::LiveEvents`] traits.
//!
//! The API is unstable. `bubblepaw-core` re-exports what embedders use, and
//! embedders should import from there.
pub mod discovery;
pub mod dispatch;
pub mod execution_mode;
pub mod extensions;
pub mod ids;
pub mod live_events;
pub mod loop_hooks;
pub mod loop_options;
pub mod loop_records;
pub mod model_stream;
pub mod observer;
pub mod overflow;
pub mod prepare_types;
pub mod reminder_types;
pub mod request_seam;
pub mod round_reminders;
pub mod run_data;
pub mod run_inputs;
pub mod run_journal;
pub mod run_loop;
pub mod run_settle;
pub mod script;
pub mod stream;
pub mod submit_result;
pub mod tool;
pub mod tool_gate;
pub mod tool_notices;
pub mod tool_registry;
pub mod tool_result;
pub mod tool_source;
pub mod tool_views;

pub use discovery::{search_tool_definitions_with_servers, tool_definition_summary};
pub use dispatch::ToolDispatcher;
pub use execution_mode::ExecutionMode;
pub use extensions::Extensions;
pub use run_data::{OutputRecord, Run};
pub use script::SCRIPT_MODEL;
pub use tool::{Tool, ToolContext};
pub use tool_registry::{ToolRegistry, ToolResultLimiter};
pub use tool_result::ToolResult;
