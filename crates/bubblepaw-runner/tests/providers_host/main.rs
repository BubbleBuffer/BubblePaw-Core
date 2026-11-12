//! Integration tests of one area (providers_host), one binary: each former test file is a module here.

#[path = "../support/chat_messages.rs"]
mod chat_messages;
#[path = "../support/worker_jobs.rs"]
mod worker_jobs;

mod aux_usage;
mod llama_config;
mod net;
mod opencode_console;
mod provider_accounts;
mod provider_ext;
mod provider_host_notices;
mod provider_live;
mod provider_quota;
mod provider_reminders;
mod provider_retry;
mod provider_setup_via;
mod quota_cli;
mod quota_rejection_evidence;
mod quota_retry_nesting;
mod usage;
mod usage_cli;
