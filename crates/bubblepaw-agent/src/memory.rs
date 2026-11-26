//! In-memory implementations of the loop's seams, for embedders who want the run
//! loop without persistence. Nothing here is written anywhere: the journal keeps the
//! transcript in a mutex, the request context builds each request from it, the gate
//! allows every call, the tools are a fixed registry and live events are collected.
//!
//! [`MemoryHost`] assembles them into a [`crate::run_inputs::RunLoop`] and runs one
//! prompt per call over a shared transcript.
mod context;
mod host;
mod journal;
mod seams;

pub use context::MemoryContext;
pub use host::MemoryHost;
pub use journal::{JournalEvent, MemoryJournal};
pub use seams::{AllowAll, CollectedEvents, FixedTools, NoLiveEvents, NoObserver};
