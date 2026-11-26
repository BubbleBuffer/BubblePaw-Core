//! Integration tests of one area (providers_wire), one binary: each former test file is a module here.

#[path = "../support/chat_messages.rs"]
mod chat_messages;
#[path = "../support/eventually.rs"]
mod eventually;
#[path = "../support/http_server.rs"]
mod http_server;
#[path = "../support/worker_jobs.rs"]
mod worker_jobs;

mod decision_providers;
mod incomplete_replies;
mod provider_chat;
mod provider_continuation;
mod provider_images;
mod provider_messages;
mod provider_reserved;
mod provider_responses;
mod provider_responses_tools_off;
mod provider_scripted;
mod provider_stream;
mod provider_structured;
mod provider_usage_parsing;
mod reasoning_config;
mod reasoning_replay;
mod reasoning_storage;
mod reasoning_thinking;
mod reasoning_tool_loops;
mod reply_facts;
mod tool_error_prefix;
mod tools_off_budget_wire;
