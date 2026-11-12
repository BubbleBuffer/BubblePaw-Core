//! Integration tests of one area (context, chapters, episodes and usage), one binary: each former test file is a module here.

#[path = "../support/mod.rs"]
mod support;

mod cache_accounting;
mod cold_notes;
mod context_chapters;
mod context_checkpoint;
mod context_episodes;
mod context_fork;
mod context_state;
mod history_cursor;
mod prompt_log;
mod request_template;
mod request_usage;
mod session_fork_at;
mod transcript_import;
mod usage_compatibility;
