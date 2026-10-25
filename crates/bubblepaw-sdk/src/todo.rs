//! The todo tool and its host state.
//!
//! The model replaces the whole list with `todo({items: [{text, status}]})`;
//! the latest list per chat is host state under [`HOST_STATE_KEY`], never part
//! of the prompt. Register the tool with [`crate::HostBuilder::todo`] (or
//! [`register`] on your own registry), read the list with [`list`] or
//! [`crate::Host::todo`], and follow changes as
//! `EventKind::HostStateChanged { key: "todo", value }`, whose `value` is the
//! bare item array.

pub use bubblepaw_runner::todo::{MAX_TODO_ITEMS, TodoItem, TodoStatus, list, register};

/// The host-state key the list is stored and announced under.
pub const HOST_STATE_KEY: &str = "todo";
