//! Integration tests of one area (admission, notices, reminders and repo checks), one binary: each former test file is a module here.

mod active_admission;
mod file_size;
mod host_notices;
mod host_writer;
mod layering;
mod layering_agent;
mod panic_in_run;
mod partial_replies;
mod reminder_runtime;
mod reminder_store;
mod reminders;
mod thinking;
