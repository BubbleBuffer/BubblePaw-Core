//! Integration tests of one area (episode), one binary: each former test file is a module here.

#[path = "../support/chat_messages.rs"]
mod chat_messages;
#[path = "../support/episode_fixture.rs"]
mod episode_fixture;
#[path = "../common/hook.rs"]
mod hook;

mod episode_background;
mod episode_background_lifecycle;
mod episode_forks;
mod episode_forks_extension;
mod episode_forks_http;
mod episode_forks_tool_loop;
mod episode_forks_wire;
mod episode_models;
mod episode_prompt;
mod episode_summary_retry;
mod episode_worker;
mod episodes;
mod side_call_forks;
mod untraced_fork;
