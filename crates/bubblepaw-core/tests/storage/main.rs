//! Integration tests of one area (stores, layout, retention and sessions), one binary: each former test file is a module here.

#[path = "../support/mod.rs"]
mod support;

mod attachment_prune;
mod catalog_rebuild_race;
mod pending_session_lookup;
mod root_storage_policy;
mod schema_version;
mod session_admin;
mod session_workspaces;
mod storage_cold;
mod storage_layout;
mod storage_round_refs;
mod storage_view_sources;
mod store_scoped;
mod store_storage;
