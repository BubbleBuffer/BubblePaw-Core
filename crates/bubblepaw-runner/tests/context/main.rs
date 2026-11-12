//! Integration tests of one area (context), one binary: each former test file is a module here.

#[path = "../support/chat_messages.rs"]
mod chat_messages;
#[path = "../support/episode_fixture.rs"]
mod episode_fixture;
#[path = "../common/hook.rs"]
mod hook;

mod chapter_prompt;
mod context_budget_recovery;
mod context_growth;
mod context_routes;
mod context_stdio;
mod context_strategies;
mod discovery_snapshot;
mod lcm_decider;
mod manual_compaction;
mod reminder_window;
mod spill;
mod storage_context;
mod summarization;
mod wire_order;
