//! Store schema versions. Each store file is created at its final schema in one
//! transaction and opens only at exactly that version. No earlier layout is
//! upgraded: a file at any other version fails with [`StoreSchemaError`].
//!
//! To change a schema later, add the new DDL to the file's creation step, raise
//! its version constant, and write the upgrade for the old version as a numbered
//! step that runs before the version check passes. Until then there is none.

use anyhow::Result;

/// Version of the chat store written by this build.
pub(super) const CHAT_VERSION: i64 = 12;
/// Version of `catalog.db`.
pub(super) const CATALOG_VERSION: i64 = 2;
/// Version of `serving.db`.
pub(super) const SERVING_VERSION: i64 = 2;

/// A store file whose schema version is not the one this build creates. The
/// file is left as it was; this build has no upgrade path for any other version,
/// older or newer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreSchemaError {
    /// Which store: `chat`, `catalog`, `serving` or `delegation ledger`.
    pub store: &'static str,
    pub found: i64,
    pub expected: i64,
}

impl StoreSchemaError {
    /// `Ok` when `found` is `expected`; otherwise this error, wrapped in `anyhow`
    /// so callers match it with `err.downcast_ref::<StoreSchemaError>()`.
    pub fn ensure(store: &'static str, found: i64, expected: i64) -> Result<()> {
        if found == expected {
            return Ok(());
        }
        Err(anyhow::Error::new(Self {
            store,
            found,
            expected,
        }))
    }
}

impl std::fmt::Display for StoreSchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "The {} store is at schema version {}, but this build requires version {}; no upgrade path exists, so the store cannot be opened",
            self.store, self.found, self.expected
        )
    }
}

impl std::error::Error for StoreSchemaError {}

/// Create the chat store at `CHAT_VERSION`, with its application id and version
/// set in the same transaction as the tables. `conn` must be an empty database.
pub(super) fn create_chat(conn: &mut rusqlite::Connection) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute_batch(CHAT_SCHEMA)?;
    tx.execute_batch(super::CHECKPOINT_SCHEMA)?;
    tx.execute_batch(&format!("PRAGMA user_version={CHAT_VERSION};"))?;
    tx.commit()?;
    Ok(())
}

/// The chat store at `CHAT_VERSION`. Tables created per open without a version
/// bump (approvals, event log, host state, reminders, dictionaries)
/// are not listed here. Their `CREATE ... IF NOT EXISTS` statements run on every
/// open, after the creation step.
pub(super) const CHAT_SCHEMA: &str = "PRAGMA application_id=1112556080;
CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE sessions(id TEXT PRIMARY KEY, data TEXT NOT NULL);
CREATE TABLE messages(seq INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL REFERENCES sessions(id), data TEXT NOT NULL, message_id TEXT);
CREATE INDEX messages_session ON messages(session_id,seq);
CREATE INDEX messages_id ON messages(session_id,message_id);
CREATE TABLE runs(id TEXT PRIMARY KEY,session_id TEXT NOT NULL REFERENCES sessions(id),status TEXT NOT NULL,data TEXT NOT NULL, result_message_id TEXT);
CREATE UNIQUE INDEX one_running ON runs(session_id) WHERE status IN ('running','waiting_for_quota');
CREATE TABLE inputs(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE NOT NULL,session_id TEXT NOT NULL REFERENCES sessions(id),kind TEXT NOT NULL,text TEXT NOT NULL,state TEXT NOT NULL, origin TEXT, message_id TEXT, content_blob TEXT, content_format TEXT);
CREATE INDEX inputs_pending ON inputs(session_id,state,seq);
CREATE TABLE effects(id TEXT PRIMARY KEY,session_id TEXT NOT NULL,run_id TEXT NOT NULL,call_id TEXT NOT NULL,state TEXT NOT NULL, detail TEXT);
CREATE TABLE outputs(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE NOT NULL,session_id TEXT NOT NULL REFERENCES sessions(id),execution_id TEXT NOT NULL,data TEXT NOT NULL);
CREATE INDEX outputs_session ON outputs(session_id,seq);
CREATE INDEX outputs_execution ON outputs(execution_id);
CREATE TABLE context_checkpoints(session_id TEXT PRIMARY KEY REFERENCES sessions(id),id TEXT UNIQUE NOT NULL,through_message_seq INTEGER NOT NULL REFERENCES messages(seq),data TEXT NOT NULL);
CREATE TABLE context_chapters(session_id TEXT NOT NULL REFERENCES sessions(id),id TEXT PRIMARY KEY,ordinal INTEGER NOT NULL CHECK(ordinal>0),first_message_seq INTEGER NOT NULL REFERENCES messages(seq),last_message_seq INTEGER NOT NULL REFERENCES messages(seq),data TEXT NOT NULL,UNIQUE(session_id,ordinal),CHECK(first_message_seq<=last_message_seq));
CREATE TABLE context_subject_chapters(id TEXT PRIMARY KEY,session_id TEXT NOT NULL REFERENCES sessions(id),ordinal INTEGER NOT NULL CHECK(ordinal>0),first_episode_ordinal INTEGER NOT NULL,last_episode_ordinal INTEGER NOT NULL,status TEXT NOT NULL CHECK(status IN ('open','closed')),data TEXT NOT NULL,UNIQUE(session_id,ordinal),CHECK(first_episode_ordinal>0 AND first_episode_ordinal<=last_episode_ordinal));
CREATE UNIQUE INDEX one_open_subject_chapter ON context_subject_chapters(session_id) WHERE status='open';
CREATE TABLE context_episodes(id TEXT PRIMARY KEY,session_id TEXT NOT NULL REFERENCES sessions(id),ordinal INTEGER NOT NULL CHECK(ordinal>0),first_message_seq INTEGER NOT NULL REFERENCES messages(seq),last_message_seq INTEGER NOT NULL REFERENCES messages(seq),chapter_id TEXT NOT NULL REFERENCES context_subject_chapters(id),data TEXT NOT NULL,UNIQUE(session_id,ordinal),CHECK(first_message_seq<=last_message_seq));
CREATE INDEX episodes_chapter ON context_episodes(chapter_id,ordinal);
CREATE TABLE context_decisions(id TEXT PRIMARY KEY,session_id TEXT NOT NULL REFERENCES sessions(id),episode_ordinal INTEGER NOT NULL,kind TEXT NOT NULL CHECK(kind='split'),data TEXT NOT NULL,FOREIGN KEY(session_id,episode_ordinal) REFERENCES context_episodes(session_id,ordinal));
CREATE INDEX decisions_session ON context_decisions(session_id,episode_ordinal);
CREATE TABLE context_views(session_id TEXT PRIMARY KEY REFERENCES sessions(id),data TEXT NOT NULL);
CREATE TABLE blobs(hash TEXT PRIMARY KEY,raw_bytes INTEGER NOT NULL,data BLOB NOT NULL);
CREATE TABLE attachments(session_id TEXT NOT NULL REFERENCES sessions(id),id TEXT NOT NULL,metadata TEXT NOT NULL,blob_hash TEXT NOT NULL REFERENCES blobs(hash),PRIMARY KEY(session_id,id));
CREATE TABLE context_checkpoint_versions(id TEXT PRIMARY KEY,session_id TEXT NOT NULL REFERENCES sessions(id),summary_hash TEXT NOT NULL REFERENCES blobs(hash));
CREATE TABLE request_usage(
    request_id TEXT PRIMARY KEY,
    created_at TEXT NOT NULL,
    session_id TEXT NOT NULL,
    run_id TEXT,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('main','summary_fork','decider')),
    input_tokens INTEGER NOT NULL CHECK(input_tokens>=0),
    cache_read_tokens INTEGER NOT NULL CHECK(cache_read_tokens>=0),
    cache_creation_tokens INTEGER NOT NULL CHECK(cache_creation_tokens>=0),
    output_tokens INTEGER NOT NULL CHECK(output_tokens>=0),
    reasoning_tokens INTEGER CHECK(reasoning_tokens>=0),
    failed INTEGER NOT NULL CHECK(failed IN (0,1)),
    account TEXT NOT NULL DEFAULT '',
    message_seq INTEGER
);
CREATE INDEX request_usage_session_time ON request_usage(session_id,created_at);
CREATE INDEX request_usage_time ON request_usage(created_at);
CREATE INDEX request_usage_message ON request_usage(session_id,message_seq) WHERE message_seq IS NOT NULL;
CREATE TABLE context_events(
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('episode_written','chapter_closed','applied')),
    data TEXT NOT NULL
);
CREATE INDEX context_events_session ON context_events(session_id);
CREATE TABLE message_tool_calls(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,call_id TEXT NOT NULL,message_seq INTEGER NOT NULL REFERENCES messages(seq) ON DELETE CASCADE,kind TEXT NOT NULL CHECK(kind IN ('call','result')),PRIMARY KEY(session_id,call_id,message_seq,kind));
CREATE INDEX message_tool_calls_sequence ON message_tool_calls(session_id,message_seq);
CREATE TABLE message_headers(message_seq INTEGER PRIMARY KEY REFERENCES messages(seq) ON DELETE CASCADE,session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,role TEXT NOT NULL,origin TEXT,typed_user INTEGER NOT NULL,created_at TEXT NOT NULL,is_error INTEGER NOT NULL);
CREATE INDEX message_headers_session ON message_headers(session_id,message_seq);";
