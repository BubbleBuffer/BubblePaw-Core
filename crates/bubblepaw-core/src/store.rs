use crate::{ContextEvent, ContextEventKind};
use crate::{
    ContextStrategy, Message, MessageOrigin, OutputRecord, RequestUsage, Role, Run, Session,
    ToolCall, ToolResult, Usage, id, is_typed_user, now,
};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex, OnceLock, Weak},
};

pub use crate::context_facts::{ContextCheckpoint, ContextView};

#[derive(Clone)]
pub struct Store(Arc<Inner>);
/// Host cleanup after canonical deletion. Filesystem cleanup is not a SQL transaction.
pub type SessionCleanup = Arc<dyn Fn(&str) -> Result<()> + Send + Sync>;
#[path = "store_chapters.rs"]
mod chapters;
pub use chapters::{ChapterSection, ContextSubjectChapter};
#[path = "store_cold.rs"]
mod cold;
#[path = "store_context_events.rs"]
mod context_events;
#[path = "store_episodes.rs"]
mod episodes;
#[path = "store_execution_mode.rs"]
mod execution_mode;
#[path = "store_layout.rs"]
mod layout;
#[path = "store_sessions.rs"]
mod sessions;
#[path = "store_tool_surface.rs"]
pub(crate) mod tool_surface;
pub use layout::{ChatOpenFailure, ChatOpenHook, RootStoragePolicy, StoreOptions};
#[path = "store_quota.rs"]
mod quota;
#[path = "store_search.rs"]
mod search;
#[path = "store_serving.rs"]
mod serving;
#[path = "store_usage.rs"]
pub(crate) mod usage;
pub use search::{SearchHit, SearchIndexProgress};
#[path = "store_import.rs"]
mod import;
#[path = "store_inputs.rs"]
mod inputs;
#[path = "store_legacy.rs"]
mod legacy;
pub use legacy::{
    LEGACY_IMPORT_BATCH, LegacyImportReport, LegacyImportStatus, LegacyImportSummary,
    LegacySnapshot,
};
#[path = "store_ownership.rs"]
mod ownership;
#[path = "store_recovery.rs"]
mod recovery;
#[path = "store_reminders.rs"]
mod reminders;
#[cfg(test)]
#[path = "store_test_support.rs"]
mod test_support;
use ownership::{FileLock, Owner, OwnershipLease};
struct Inner {
    connection: Arc<Mutex<Connection>>,
    dictionary_pins: cold::DictionaryPins,
    layout: Option<Arc<layout::Layout>>,
    host: Option<Weak<Inner>>,
    path: Option<PathBuf>,
    transactions: Arc<AtomicU64>,
    write_nanoseconds: Arc<AtomicU64>,
    runtime: Arc<crate::runtime::RuntimeState>,
    pending_usage: Arc<Mutex<Vec<RequestUsage>>>,
    // Last: flush and drop all store state before releasing ownership.
    _lock: Option<OwnershipLease>,
}

type Owners = Mutex<HashMap<PathBuf, Owner>>;
type InputContentRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);
static OWNERS: OnceLock<Owners> = OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Steer,
    Followup,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedInput {
    pub id: String,
    pub session_id: String,
    pub kind: InputKind,
    pub text: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<MessageOrigin>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageStats {
    pub page_bytes: u64,
    pub database_bytes: u64,
    pub wal_bytes: u64,
    /// Committed application write transactions since this Store was opened.
    /// New-database schema initialization is not part of this counter.
    pub transaction_count: u64,
    pub message_count: u64,
    pub pending_input_count: u64,
    pub chat_bytes: u64,
    pub serving_bytes: u64,
    pub catalog_bytes: u64,
    pub chat_transaction_count: u64,
    pub serving_transaction_count: u64,
    pub catalog_transaction_count: u64,
}

/// A coherent, bounded read of canonical history. Counts describe the entire
/// session; messages and runs contain the newest records in chronological order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub revision: u64,
    pub session: Session,
    pub messages: Vec<Message>,
    pub runs: Vec<Run>,
    pub pending_inputs: Vec<QueuedInput>,
    pub message_count: u64,
    pub run_count: u64,
    pub pending_input_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session: Session,
    pub message_count: u64,
    pub last_message: Option<Message>,
    pub latest_run: Option<Run>,
    pub pending_input_count: u64,
}

// The public view remains a rendered request. Its durable representation contains
// canonical ranges and generated-message metadata with source text references.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PrefixPointer {
    Messages {
        first: i64,
        last: i64,
    },
    Rendered {
        base_seq: Option<i64>,
        metadata: serde_json::Value,
        content: Vec<TextPointer>,
        fields: HashMap<String, Vec<TextPointer>>,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TextPointer {
    Literal {
        text: String,
    },
    Source {
        table: String,
        id: String,
        field: String,
        start: usize,
        end: usize,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ContentPart {
    Literal { text: String },
    Blob { hash: String, json_escape: bool },
}

struct TextSourceRef {
    table: String,
    id: String,
    field: String,
}
struct ViewTextMatch {
    source: Arc<TextSourceRef>,
    start: usize,
    length: usize,
}
struct ViewSources {
    texts: Vec<String>,
    matches: Vec<std::collections::BTreeMap<usize, ViewTextMatch>>,
    coverage: Vec<std::collections::BTreeMap<usize, usize>>,
    needles: HashMap<[u8; 16], Vec<(usize, usize)>>,
}

/// An immutable summary of a contiguous range of canonical session messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextChapter {
    pub id: String,
    pub session_id: String,
    /// One-based append order within this session.
    pub ordinal: usize,
    pub first_message_id: String,
    pub last_message_id: String,
    pub title: String,
    pub summary: String,
    pub model: String,
    pub usage: Option<Usage>,
    pub created_at: String,
}

/// A write-once summary of a consecutive range of canonical messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextEpisode {
    pub id: String,
    pub session_id: String,
    pub ordinal: usize,
    pub first_message_id: String,
    pub last_message_id: String,
    pub source_tokens: usize,
    /// A summary length target, not an enforced output limit.
    pub cap: usize,
    pub origin_request_id: Option<String>,
    pub title: String,
    pub summary: String,
    pub model: String,
    pub usage: Option<Usage>,
    #[serde(default)]
    pub reasoning: Option<String>,
    pub created_at: String,
    pub chapter_id: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubjectChapterStatus {
    Open,
    Closed,
}

/// Exact request or delegated-task words offered for a related chapter link.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RelatedEvidence {
    pub quote: String,
    pub related_quote_matched: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<crate::MessageOrigin>,
}

impl<'de> Deserialize<'de> for RelatedEvidence {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Evidence {
            Legacy(String),
            Detailed {
                quote: String,
                #[serde(default)]
                related_quote_matched: Option<bool>,
                #[serde(default)]
                origin: Option<crate::MessageOrigin>,
            },
        }
        Ok(match Evidence::deserialize(deserializer)? {
            Evidence::Legacy(quote) => Self {
                quote,
                related_quote_matched: None,
                origin: None,
            },
            Evidence::Detailed {
                quote,
                related_quote_matched,
                origin,
            } => Self {
                quote,
                related_quote_matched,
                origin,
            },
        })
    }
}

impl From<String> for RelatedEvidence {
    fn from(quote: String) -> Self {
        Self {
            quote,
            related_quote_matched: None,
            origin: None,
        }
    }
}

impl From<&str> for RelatedEvidence {
    fn from(quote: &str) -> Self {
        Self::from(quote.to_owned())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    Split,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextDecision {
    pub id: String,
    pub session_id: String,
    pub kind: DecisionKind,
    pub episode_ordinal: usize,
    pub option_message_ids: Vec<String>,
    /// In option order, followed by the probability of choosing none.
    pub probabilities: Vec<f64>,
    pub choice: Option<String>,
    pub model: String,
    pub usage: Option<Usage>,
    #[serde(default)]
    pub reasoning: Option<String>,
    pub created_at: String,
}

/// One episode and its chapter transition, committed together or not at all.
#[derive(Debug, Clone)]
pub struct EpisodeBatch {
    pub episode: ContextEpisode,
    pub decisions: Vec<ContextDecision>,
    /// The open chapter membership after appending this episode, before any close.
    pub open_chapter: ContextSubjectChapter,
    /// The previous open chapter's final metadata, when starting a new chapter.
    pub closed_chapter: Option<ContextSubjectChapter>,
    /// Final metadata for closing this episode's chapter after inserting it.
    /// Cannot be combined with `closed_chapter`.
    pub close_after_episode: Option<ContextSubjectChapter>,
    /// Last covered message ID observed by the caller; None means no coverage.
    pub expected_previous_coverage: Option<String>,
}

impl Store {
    /// One process owns a database. Repeated opens in this process share the connection.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if path.is_dir() {
            return Self::open_data_dir(path);
        }
        let data_dir = Self::legacy_data_dir(path);
        Self::migrate_legacy(path, &data_dir)?;
        Self::open_data_dir(data_dir)
    }
    pub fn in_memory() -> Result<Self> {
        Self::initialize(Connection::open_in_memory()?, None)
    }
    fn initialize(mut connection: Connection, path: Option<PathBuf>) -> Result<Self> {
        const APPLICATION_ID: i64 = 0x42504230;
        let application: i64 = connection.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        let tables: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )?;
        anyhow::ensure!(
            application == APPLICATION_ID || (application == 0 && tables == 0),
            "Foreign SQLite database; refusing to initialize or recover it"
        );
        let version: i64 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        anyhow::ensure!(
            version <= 12,
            "Unsupported database schema version {version}"
        );
        if tables == 0 {
            connection.execute_batch("PRAGMA auto_vacuum=INCREMENTAL;")?;
        }
        connection.busy_timeout(std::time::Duration::from_secs(2))?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA wal_autocheckpoint=256; PRAGMA journal_size_limit=1048576;")?;
        if application == 0 {
            let initialization = connection.transaction()?;
            initialization.execute_batch("PRAGMA application_id=1112556080; PRAGMA user_version=1;
          CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY, data TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS messages(seq INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL REFERENCES sessions(id), data TEXT NOT NULL);
          CREATE INDEX IF NOT EXISTS messages_session ON messages(session_id,seq);
          CREATE TABLE IF NOT EXISTS runs(id TEXT PRIMARY KEY,session_id TEXT NOT NULL REFERENCES sessions(id),status TEXT NOT NULL,data TEXT NOT NULL);
          CREATE UNIQUE INDEX IF NOT EXISTS one_running ON runs(session_id) WHERE status IN ('running','waiting_for_quota');
          CREATE TABLE IF NOT EXISTS inputs(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE NOT NULL,session_id TEXT NOT NULL REFERENCES sessions(id),kind TEXT NOT NULL,text TEXT NOT NULL,state TEXT NOT NULL);
          CREATE INDEX IF NOT EXISTS inputs_pending ON inputs(session_id,state,seq);
          CREATE TABLE IF NOT EXISTS effects(id TEXT PRIMARY KEY,session_id TEXT NOT NULL,run_id TEXT NOT NULL,call_id TEXT NOT NULL,state TEXT NOT NULL);")?;
            initialization.commit()?;
        } else {
            anyhow::ensure!(
                (1..=12).contains(&version),
                "Unsupported database schema version {version}"
            );
        }
        if version < 2 {
            let migration = connection.transaction()?;
            migration.execute_batch("ALTER TABLE effects ADD COLUMN detail TEXT; CREATE TABLE outputs(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE NOT NULL,session_id TEXT NOT NULL REFERENCES sessions(id),execution_id TEXT NOT NULL,data TEXT NOT NULL); CREATE INDEX outputs_session ON outputs(session_id,seq); CREATE INDEX outputs_execution ON outputs(execution_id); PRAGMA user_version=2;")?;
            migration.commit()?;
        }
        if version < 3 {
            let migration = connection.transaction()?;
            migration.execute_batch("CREATE TABLE context_checkpoints(session_id TEXT PRIMARY KEY REFERENCES sessions(id),id TEXT UNIQUE NOT NULL,through_message_seq INTEGER NOT NULL REFERENCES messages(seq),data TEXT NOT NULL); PRAGMA user_version=3;")?;
            migration.commit()?;
        }
        if version < 4 {
            let migration = connection.transaction()?;
            migration.execute_batch("CREATE TABLE context_chapters(session_id TEXT NOT NULL REFERENCES sessions(id),id TEXT PRIMARY KEY,ordinal INTEGER NOT NULL CHECK(ordinal>0),first_message_seq INTEGER NOT NULL REFERENCES messages(seq),last_message_seq INTEGER NOT NULL REFERENCES messages(seq),data TEXT NOT NULL,UNIQUE(session_id,ordinal),CHECK(first_message_seq<=last_message_seq)); PRAGMA user_version=4;")?;
            migration.commit()?;
        }
        if version < 5 {
            let migration = connection.transaction()?;
            migration.execute_batch("ALTER TABLE inputs ADD COLUMN origin TEXT;
            CREATE TABLE context_subject_chapters(id TEXT PRIMARY KEY,session_id TEXT NOT NULL REFERENCES sessions(id),ordinal INTEGER NOT NULL CHECK(ordinal>0),first_episode_ordinal INTEGER NOT NULL,last_episode_ordinal INTEGER NOT NULL,status TEXT NOT NULL CHECK(status IN ('open','closed')),data TEXT NOT NULL,UNIQUE(session_id,ordinal),CHECK(first_episode_ordinal>0 AND first_episode_ordinal<=last_episode_ordinal));
            CREATE UNIQUE INDEX one_open_subject_chapter ON context_subject_chapters(session_id) WHERE status='open';
            CREATE TABLE context_episodes(id TEXT PRIMARY KEY,session_id TEXT NOT NULL REFERENCES sessions(id),ordinal INTEGER NOT NULL CHECK(ordinal>0),first_message_seq INTEGER NOT NULL REFERENCES messages(seq),last_message_seq INTEGER NOT NULL REFERENCES messages(seq),chapter_id TEXT NOT NULL REFERENCES context_subject_chapters(id),data TEXT NOT NULL,UNIQUE(session_id,ordinal),CHECK(first_message_seq<=last_message_seq));
            CREATE INDEX episodes_chapter ON context_episodes(chapter_id,ordinal);
            CREATE TABLE context_decisions(id TEXT PRIMARY KEY,session_id TEXT NOT NULL REFERENCES sessions(id),episode_ordinal INTEGER NOT NULL,kind TEXT NOT NULL CHECK(kind='split'),data TEXT NOT NULL,FOREIGN KEY(session_id,episode_ordinal) REFERENCES context_episodes(session_id,ordinal));
            CREATE INDEX decisions_session ON context_decisions(session_id,episode_ordinal);
            PRAGMA user_version=5;")?;
            migration.commit()?;
        }
        if version < 6 {
            let migration = connection.transaction()?;
            migration.execute_batch("CREATE TABLE context_views(session_id TEXT PRIMARY KEY REFERENCES sessions(id),data TEXT NOT NULL); PRAGMA user_version=6;")?;
            migration.commit()?;
        }
        connection.execute_batch(crate::approvals::SCHEMA)?;
        if version < 7 {
            let migration = connection.transaction()?;
            migration.execute_batch("ALTER TABLE messages ADD COLUMN message_id TEXT;
                UPDATE messages SET message_id=json_extract(data,'$.id');
                CREATE INDEX messages_id ON messages(session_id,message_id);
                CREATE TABLE blobs(hash TEXT PRIMARY KEY,raw_bytes INTEGER NOT NULL,data BLOB NOT NULL);
                CREATE TABLE attachments(session_id TEXT NOT NULL REFERENCES sessions(id),id TEXT NOT NULL,metadata TEXT NOT NULL,blob_hash TEXT NOT NULL REFERENCES blobs(hash),PRIMARY KEY(session_id,id));
                ALTER TABLE inputs ADD COLUMN message_id TEXT;
                ALTER TABLE inputs ADD COLUMN content_blob TEXT;
                ALTER TABLE inputs ADD COLUMN content_format TEXT;
                ALTER TABLE runs ADD COLUMN result_message_id TEXT;
                ALTER TABLE approvals ADD COLUMN arguments_ref TEXT;
                CREATE TABLE context_checkpoint_versions(id TEXT PRIMARY KEY,session_id TEXT NOT NULL REFERENCES sessions(id),summary_hash TEXT NOT NULL REFERENCES blobs(hash));
                PRAGMA user_version=7;")?;
            migration.commit()?;
        }
        Self::migrate_request_usage(&mut connection, version)?;
        // Additive tables without a version bump (docs/approvals.md).
        connection.execute_batch(crate::approvals::SCHEMA)?;
        connection.execute_batch(crate::event_log::SCHEMA)?;
        connection.execute_batch(crate::host_state::SCHEMA)?;
        Self::initialize_reminders(&connection)?;
        migrate_checkpoint_schema(&mut connection, version)?;
        cold::initialize(&connection)?;
        let dictionary_pins = cold::pin_dictionaries(&connection)?;
        Self::initialize_tool_call_index(&mut connection)?;
        Self::migrate_context_facts(&mut connection, version)?;
        let (last_event, pruned_events) = crate::event_log::bounds(&connection)?;
        let store = Self(Arc::new(Inner {
            connection: Arc::new(Mutex::new(connection)),
            dictionary_pins,
            layout: None,
            host: None,
            path,
            transactions: Arc::new(AtomicU64::new(0)),
            write_nanoseconds: Arc::new(AtomicU64::new(0)),
            runtime: Arc::default(),
            pending_usage: Arc::default(),
            _lock: None,
        }));
        store.0.runtime.log.seed(last_event, pruned_events);
        store.recover()?;
        Ok(store)
    }
    pub(crate) fn runtime(&self) -> Arc<crate::runtime::RuntimeState> {
        self.0.runtime.clone()
    }
    /// Legacy default for chats without an authoritative root workspace.
    pub fn bound_workspace(&self) -> Result<Option<std::path::PathBuf>> {
        let root: Option<String> = if self.is_layout() {
            let layout = self.0.layout.as_ref().unwrap();
            let catalog = layout.catalog.lock().unwrap();
            catalog
                .query_row(
                    "SELECT value FROM metadata WHERE key='workspace'",
                    [],
                    |row| row.get(0),
                )
                .optional()?
        } else {
            let conn = self.0.connection.lock().unwrap();
            conn.query_row(
                "SELECT value FROM metadata WHERE key='workspace'",
                [],
                |row| row.get(0),
            )
            .optional()?
        };
        root.map(|root| Self::workspace_binding(&root)).transpose()
    }
    /// The root chat's workspace, inherited by all of its child sessions.
    /// Reads old bindings without rewriting the chat or restoring an archive.
    pub fn session_workspace(&self, session_id: &str) -> Result<Option<PathBuf>> {
        if self.is_layout() {
            return self.layout_session_workspace(session_id);
        }
        if let Some(host) = self.host() {
            return host.session_workspace(session_id);
        }
        let workspace =
            self.read(|conn| Self::session_workspace_connection(conn, session_id, None))?;
        match workspace {
            Some(workspace) => Ok(Some(workspace)),
            None => self.bound_workspace(),
        }
    }
    /// Physical root-chat identity, including archived chats. This metadata read
    /// never opens a chat file, restores an archive, or rewrites legacy ancestry.
    pub fn session_root_id(&self, session_id: &str) -> Result<String> {
        if let Some(layout) = &self.0.layout {
            if let Some((root, _, _)) = layout.pending_catalog.lock().unwrap().get(session_id) {
                return Ok(root.clone());
            }
            return Ok(layout.catalog.lock().unwrap().query_row(
                "SELECT root_id FROM sessions WHERE id=?1",
                [session_id],
                |row| row.get(0),
            )?);
        }
        if let Some(host) = self.host() {
            return host.session_root_id(session_id);
        }
        self.read(|conn| Self::session_root_connection(conn, session_id, None))
    }
    /// A saved policy is inherited from the physical root chat. None preserves
    /// the host's legacy defaults, without writing policy metadata into old chats.
    pub fn session_storage_policy(&self, session_id: &str) -> Result<Option<RootStoragePolicy>> {
        if self.is_layout() {
            return self.layout_session_storage_policy(session_id);
        }
        if let Some(host) = self.host() {
            return host.session_storage_policy(session_id);
        }
        self.read(|conn| Self::session_storage_connection(conn, session_id, None))
    }
    fn session_storage_connection(
        conn: &Connection,
        session_id: &str,
        root_id: Option<&str>,
    ) -> Result<Option<RootStoragePolicy>> {
        let root = Self::session_root_connection(conn, session_id, root_id)?;
        let value: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [format!("root_storage:{root}")],
                |row| row.get(0),
            )
            .optional()?;
        value
            .map(|value| {
                let policy: RootStoragePolicy =
                    serde_json::from_str(&value).context("Malformed root storage policy")?;
                policy.validate()?;
                Ok(policy)
            })
            .transpose()
    }
    fn workspace_binding(value: &str) -> Result<PathBuf> {
        let path = PathBuf::from(value);
        // Stored paths remain readable when a chat is moved between Windows
        // and Unix. Execution still requires a directory on the current host.
        let bytes = value.as_bytes();
        let windows_drive = bytes.first().is_some_and(u8::is_ascii_alphabetic)
            && bytes.get(1) == Some(&b':')
            && bytes
                .get(2)
                .is_some_and(|byte| matches!(*byte, b'/' | b'\\'));
        let windows_unc = value.strip_prefix("\\\\").is_some_and(|tail| {
            let mut components = tail.split(['\\', '/']);
            components.next().is_some_and(|server| !server.is_empty())
                && components.next().is_some_and(|share| !share.is_empty())
        });
        let windows_absolute = windows_drive || windows_unc;
        let normalized = if windows_absolute {
            !value
                .split(['\\', '/'])
                .any(|component| matches!(component, "." | ".."))
        } else {
            !path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
        };
        anyhow::ensure!(
            !value.contains('\0')
                && (path.is_absolute() || value.starts_with('/') || windows_absolute)
                && normalized,
            "Malformed workspace binding: expected an absolute normalized path"
        );
        Ok(path)
    }
    fn session_workspace_connection(
        conn: &Connection,
        session_id: &str,
        root_id: Option<&str>,
    ) -> Result<Option<PathBuf>> {
        let current = Self::session_root_connection(conn, session_id, root_id)?;
        let workspace: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [format!("root_workspace:{current}")],
                |row| row.get(0),
            )
            .optional()?;
        let workspace = match workspace {
            Some(workspace) => Some(workspace),
            None => conn
                .query_row(
                    "SELECT value FROM metadata WHERE key='workspace'",
                    [],
                    |row| row.get(0),
                )
                .optional()?,
        };
        workspace
            .map(|value| Self::workspace_binding(&value))
            .transpose()
    }
    fn session_root_connection(
        conn: &Connection,
        session_id: &str,
        root_id: Option<&str>,
    ) -> Result<String> {
        let mut current = session_id.to_owned();
        let mut seen = std::collections::HashSet::new();
        loop {
            anyhow::ensure!(
                seen.insert(current.clone()),
                "Session ancestry contains a cycle"
            );
            let data: String =
                conn.query_row("SELECT data FROM sessions WHERE id=?1", [&current], |row| {
                    row.get(0)
                })?;
            let session: Session = serde_json::from_str(&data)?;
            if let Some(root) = root_id {
                // Old migrated forks can have a parent in a different chat.
                // Catalog placement, rather than that historical link, owns
                // their workspace. No source chat needs rewriting.
                current = root.to_owned();
                break;
            }
            match session.parent_session_id {
                Some(parent) => current = parent,
                None => break,
            }
        }
        Ok(current)
    }
    pub fn bind_workspace(&self, path: impl AsRef<Path>) -> Result<()> {
        if self.is_layout() {
            return self.layout_bind_workspace(path.as_ref());
        }
        let root = path.as_ref().canonicalize()?.to_string_lossy().into_owned();
        {
            let conn = self.0.connection.lock().unwrap();
            let mut stmt = conn.prepare("SELECT value FROM metadata WHERE key='workspace'")?;
            let mut rows = stmt.query([])?;
            if let Some(row) = rows.next()? {
                let existing: String = row.get(0)?;
                anyhow::ensure!(
                    existing == root,
                    "Database belongs to another workspace: {existing}"
                );
                return Ok(());
            }
        }
        self.write(|tx| {
            tx.execute(
                "INSERT OR IGNORE INTO metadata VALUES('workspace',?1)",
                [&root],
            )?;
            let existing: String = tx.query_row(
                "SELECT value FROM metadata WHERE key='workspace'",
                [],
                |r| r.get(0),
            )?;
            anyhow::ensure!(
                existing == root,
                "Database belongs to another workspace: {existing}"
            );
            Ok(())
        })
    }
    #[cfg(test)]
    pub(crate) fn admit(&self, run: &Run, text: Option<&str>, origin: MessageOrigin) -> Result<()> {
        if let Some(store) = self.scoped(&run.session_id)? {
            return store.admit(run, text, origin);
        }
        self.write(|tx|{tx.execute("INSERT INTO runs(id,session_id,status,data) VALUES(?1,?2,'running',?3)",params![run.id,run.session_id,serde_json::to_string(run)?])?;if let Some(text)=text {tx.execute("INSERT INTO inputs(id,session_id,kind,text,state,origin) VALUES(?1,?2,'followup',?3,'pending',?4)",params![id(),run.session_id,text,serde_json::to_string(&origin)?])?;}Ok(())})
    }
    pub(crate) fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        f(&self.0.connection.lock().unwrap())
    }
    pub(crate) fn write<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.write_impl(f, false)
    }
    pub(crate) fn write_context<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.write_impl(f, true)
    }
    fn write_impl<T>(&self, f: impl FnOnce(&Connection) -> Result<T>, context: bool) -> Result<T> {
        if self.is_layout() {
            return self.write_serving(f);
        }
        let started = std::time::Instant::now();
        let result = (|| {
            let mut conn = self.0.connection.lock().unwrap();
            let before = if self.host().is_some() {
                conn.query_row("SELECT COALESCE(MAX(seq),0) FROM messages", [], |row| {
                    row.get::<_, i64>(0)
                })?
            } else {
                0
            };
            let tx = conn.transaction()?;
            // Retain staged records on any rollback. Only canonical chat writes
            // enter here; serving transactions never drain billing records.
            let mut pending_usage = self.0.pending_usage.lock().unwrap();
            Self::insert_request_usage_tx(&tx, &pending_usage)?;
            let value = f(&tx)?;
            let operations = self.split_operations(&tx)?;
            let telemetry = if context {
                self.split_context_telemetry(&tx)?
            } else {
                Vec::new()
            };
            tx.commit()?;
            pending_usage.clear();
            self.0.transactions.fetch_add(1, Ordering::Relaxed);
            // Search is disposable: decoding/index preparation must never undo
            // an accepted record or change success after an external effect.
            let indexed = if self.host().is_some() {
                Self::search_rows_after(&conn, before)
            } else {
                Ok(Vec::new())
            };
            Ok((value, telemetry, indexed, before, operations))
        })();
        self.0.write_nanoseconds.fetch_add(
            started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        if result.is_ok() {
            self.refresh_catalog_best_effort();
        }
        result.map(|(value, telemetry, indexed, before, operations)| {
            self.save_operations_best_effort(&operations);
            match indexed {
                Ok(indexed) => self.index_rows_best_effort(&indexed, before),
                Err(_) => self.invalidate_search_best_effort(),
            }
            self.save_telemetry_best_effort(&telemetry);
            value
        })
    }

    /// Application write-boundary wall time since opening, including lock wait,
    /// validation, commit and failed/rolled-back attempts; excludes event journal
    /// and schema initialization. Nested inside worker/compaction durations.
    pub fn write_seconds(&self) -> f64 {
        self.0.write_nanoseconds.load(Ordering::Relaxed) as f64 / 1_000_000_000.0
    }

    /// A resume may reaffirm its persisted policy, but cannot change it.
    pub fn ensure_context_strategy(
        &self,
        session_id: &str,
        requested: ContextStrategy,
    ) -> Result<()> {
        let saved = self.session(session_id)?.context_strategy;
        anyhow::ensure!(
            saved == requested,
            "Context strategy is fixed for this session ({saved:?}); create a new session to use {requested:?}"
        );
        Ok(())
    }
    pub fn session(&self, session_id: &str) -> Result<Session> {
        if let Some(store) = self.scoped(session_id)? {
            return store.session(session_id);
        }
        let conn = self.0.connection.lock().unwrap();
        let value: String =
            conn.query_row("SELECT data FROM sessions WHERE id=?1", [session_id], |r| {
                r.get(0)
            })?;
        Ok(serde_json::from_str(&value)?)
    }
    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        if self.0.layout.is_some() {
            return self.layout_sessions();
        }
        let conn = self.0.connection.lock().unwrap();
        let mut stmt = conn.prepare("SELECT data FROM sessions ORDER BY rowid DESC")?;
        stmt.query_map([], |r| r.get::<_, String>(0))?
            .map(|r| Ok(serde_json::from_str(&r?)?))
            .collect()
    }
    /// Ordinary host lists omit internal sessions; lifecycle code uses `list_sessions`.
    pub fn list_visible_sessions(&self) -> Result<Vec<Session>> {
        Ok(self
            .list_sessions()?
            .into_iter()
            .filter(|session| !session.hidden)
            .collect())
    }

    /// Bounded metadata for ordinary host projections, filtering before the limit.
    pub fn visible_session_summaries(&self, limit: usize) -> Result<Vec<SessionRecord>> {
        self.session_summaries_impl(limit, true)
    }
    /// Change a session's title; `updated_at` moves too.
    pub fn rename_session(&self, session_id: &str, title: impl Into<String>) -> Result<Session> {
        if let Some(store) = self.scoped(session_id)? {
            return store.rename_session(session_id, title);
        }
        let title = title.into();
        self.write(|tx| {
            let data: String = tx
                .query_row("SELECT data FROM sessions WHERE id=?1", [session_id], |r| {
                    r.get(0)
                })
                .optional()?
                .with_context(|| format!("Unknown session: {session_id}"))?;
            let mut session: Session = serde_json::from_str(&data)?;
            session.title = title;
            session.updated_at = now();
            tx.execute(
                "UPDATE sessions SET data=?2 WHERE id=?1",
                params![session_id, serde_json::to_string(&session)?],
            )?;
            Ok(session)
        })
    }
    /// Delete a session and everything stored for it, in one transaction:
    /// messages, runs, inputs, effects, outputs, events, approvals, approval
    /// rules, checkpoints and chapters. Refused while it has a running run.
    /// Child sessions that name it as parent are kept.
    pub fn delete_session(&self, session_id: &str) -> Result<()> {
        self.delete_session_inner(session_id, false)
    }
    /// Register idempotent cleanup for host-owned session resources. Reusing a
    /// name replaces its process-local hook; reopening requires registration again.
    pub fn set_session_cleanup(&self, name: impl Into<String>, cleanup: SessionCleanup) {
        self.runtime()
            .session_cleanups
            .lock()
            .unwrap()
            .insert(name.into(), cleanup);
    }
    /// Release one host-owned lifecycle hook without affecting other consumers.
    pub fn remove_session_cleanup(&self, name: &str) {
        self.runtime().session_cleanups.lock().unwrap().remove(name);
    }
    fn cleanup_deleted_sessions(&self, sessions: &[String]) -> Result<()> {
        let cleanups: Vec<_> = self
            .runtime()
            .session_cleanups
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect();
        let mut first_error = None;
        for session in sessions {
            for cleanup in &cleanups {
                if let Err(error) = cleanup(session) {
                    first_error.get_or_insert(error);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }
    /// Host housekeeping may delete only histories with fully resolved effects.
    /// The check shares the deletion admission guard and canonical transaction.
    pub fn delete_session_preserving_effects(&self, session_id: &str) -> Result<()> {
        self.delete_session_inner(session_id, true)
    }
    fn delete_session_inner(&self, session_id: &str, preserve_effects: bool) -> Result<()> {
        if self.0.layout.is_some() {
            return self.layout_delete(session_id, preserve_effects);
        }
        let mut removed = vec![session_id.to_owned()];
        if preserve_effects {
            let sessions = self.list_sessions()?;
            loop {
                let before = removed.len();
                for session in &sessions {
                    if session
                        .parent_session_id
                        .as_ref()
                        .is_some_and(|parent| removed.contains(parent))
                        && !removed.contains(&session.id)
                    {
                        removed.push(session.id.clone());
                    }
                }
                if removed.len() == before {
                    break;
                }
            }
        }
        let runtime = self.runtime();
        let mut admission = runtime.delete_guard(&removed)?;
        // Read before taking the connection: the log locks its state, then the connection.
        let last_event = self.0.runtime.log.last();
        self.write(|tx| {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
                [session_id],
                |r| r.get(0),
            )?;
            anyhow::ensure!(exists, "Unknown session: {session_id}");
            for session in &removed {
            let running: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM runs WHERE session_id=?1 AND status IN ('running','waiting_for_quota'))",
                [session],
                |r| r.get(0),
            )?;
            anyhow::ensure!(!running, "Cannot delete a session with a running run");
            if preserve_effects {
                let unresolved: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM effects WHERE session_id=?1 AND state!='completed')",
                    [session], |row| row.get(0),
                )?;
                anyhow::ensure!(!unresolved, "Cannot delete a session with unresolved effects");
            }
            }
            // Keep the event sequence monotonic across restarts even when the
            // newest events belong to this session.
            crate::event_log::remember_last(tx, last_event)?;
            for session in &removed {
                self.delete_session_checkpoints(tx, session)?;
                tx.execute("UPDATE request_usage SET message_seq=NULL WHERE session_id=?1 AND message_seq IS NOT NULL", [session])?;
            }
            for table in [
                "reminder_metadata",
                "host_state",
                "approvals",
                "approval_rules",
                "events",
                "attachments",
                "context_views",
                "context_checkpoint_versions",
                "context_decisions",
                "context_events",
                "context_episodes",
                "context_subject_chapters",
                "context_chapters",
                "context_checkpoints",
                "outputs",
                "effects",
                "inputs",
                "runs",
                "messages",
                "sessions",
            ] {
                let column = if table == "sessions" {
                    "id"
                } else {
                    "session_id"
                };
                for session in &removed { tx.execute(
                    &format!("DELETE FROM {table} WHERE {column}=?1"),
                    [session],
                )?; }
            }
            for session in &removed {
                Self::delete_execution_mode_tx(tx, session)?;
                Self::clear_quota_wait_tx(tx, session, None)?;
            }
            Ok(())
        })?;
        for session in &removed {
            admission.remove(session);
            runtime.forget_history(session);
        }
        self.cleanup_deleted_sessions(&removed)?;
        Ok(())
    }
    /// Process-local cache invalidation marker, not a durable replay cursor.
    pub fn revision(&self) -> u64 {
        self.0.transactions.load(Ordering::Relaxed)
    }

    pub fn snapshot(
        &self,
        session_id: &str,
        message_limit: usize,
        run_limit: usize,
        pending_limit: usize,
    ) -> Result<SessionSnapshot> {
        if let Some(store) = self.scoped(session_id)? {
            let mut snapshot =
                store.snapshot(session_id, message_limit, run_limit, pending_limit)?;
            let mut runs = store.all_runs(session_id)?;
            snapshot.run_count = runs.len() as u64;
            if runs.len() > run_limit {
                runs.drain(..runs.len() - run_limit);
            }
            snapshot.runs = runs;
            return Ok(snapshot);
        }
        let conn = self.0.connection.lock().unwrap();
        let session: String = conn.query_row(
            "SELECT data FROM sessions WHERE id=?1",
            [session_id],
            |row| row.get(0),
        )?;
        let read_json = |table: &str, order: &str, limit: usize| -> Result<Vec<String>> {
            let mut statement = conn.prepare(&format!(
                "SELECT data FROM {table} WHERE session_id=?1 ORDER BY {order} DESC LIMIT ?2"
            ))?;
            let mut records = statement
                .query_map(
                    params![session_id, limit.min(i64::MAX as usize) as i64],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            records.reverse();
            Ok(records)
        };
        let messages = read_json("messages", "seq", message_limit)?
            .into_iter()
            .map(|value| Self::message_from_data(&conn, &value))
            .collect::<Result<_>>()?;
        let runs = read_json("runs", "rowid", run_limit)?
            .into_iter()
            .map(|value| Self::run_from_data(&conn, &value))
            .collect::<Result<_>>()?;
        let mut statement = conn.prepare(
            "SELECT id,kind,text,state,origin,content_blob,content_format FROM inputs WHERE session_id=?1 AND state='pending' ORDER BY seq LIMIT ?2"
        )?;
        let pending_inputs = statement
            .query_map(
                params![session_id, pending_limit.min(i64::MAX as usize) as i64],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                    ))
                },
            )?
            .map(|row| {
                let (id, kind, text, state, origin, content_blob, content_format) = row?;
                let text = Self::input_text(
                    &conn,
                    &text,
                    content_blob.as_deref(),
                    content_format.as_deref(),
                )?;
                Ok(QueuedInput {
                    id,
                    session_id: session_id.into(),
                    kind: serde_json::from_value(serde_json::Value::String(kind))?,
                    text,
                    state,
                    origin: origin
                        .map(|value| serde_json::from_str(&value))
                        .transpose()?,
                })
            })
            .collect::<Result<_>>()?;
        let count = |table: &str, extra: &str| -> Result<u64> {
            Ok(conn.query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE session_id=?1 {extra}"),
                [session_id],
                |row| row.get::<_, i64>(0),
            )? as u64)
        };
        Ok(SessionSnapshot {
            revision: self.revision(),
            session: serde_json::from_str(&session)?,
            messages,
            runs,
            pending_inputs,
            message_count: count("messages", "")?,
            run_count: count("runs", "")?,
            pending_input_count: count("inputs", "AND state='pending'")?,
        })
    }

    /// Bounded session metadata without loading full conversations.
    pub fn session_summaries(&self, limit: usize) -> Result<Vec<SessionRecord>> {
        self.session_summaries_impl(limit, false)
    }
    fn session_summaries_impl(
        &self,
        limit: usize,
        visible_only: bool,
    ) -> Result<Vec<SessionRecord>> {
        if self.0.layout.is_some() {
            return self.layout_summaries(limit, visible_only);
        }
        let mut records = self
            .list_sessions()?
            .into_iter()
            .filter(|session| !visible_only || !session.hidden)
            .map(|session| {
                let snapshot = self.snapshot(&session.id, 1, 1, 0)?;
                Ok(SessionRecord {
                    session,
                    message_count: snapshot.message_count,
                    last_message: snapshot.messages.into_iter().next(),
                    latest_run: snapshot.runs.into_iter().next(),
                    pending_input_count: snapshot.pending_input_count,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let activity = |record: &SessionRecord| {
            let mut timestamp = record.session.updated_at.clone();
            if let Some(message) = &record.last_message {
                timestamp = timestamp.max(message.created_at.clone());
            }
            if let Some(run) = &record.latest_run {
                timestamp = timestamp.max(run.ended_at.as_ref().unwrap_or(&run.started_at).clone());
            }
            timestamp
        };
        records.sort_by_cached_key(|record| std::cmp::Reverse(activity(record)));
        records.truncate(limit);
        Ok(records)
    }
    pub fn messages(&self, session_id: &str) -> Result<Vec<Message>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.messages(session_id);
        }
        let conn = self.0.connection.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT data FROM messages WHERE session_id=?1 ORDER BY seq")?;
        stmt.query_map([session_id], |r| r.get::<_, String>(0))?
            .map(|r| Self::message_from_data(&conn, &r?))
            .collect()
    }

    /// Read only newly appended records, using the session/sequence index.
    pub fn messages_after(&self, session_id: &str, after_seq: i64) -> Result<Vec<(i64, Message)>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.messages_after(session_id, after_seq);
        }
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT seq,data FROM messages WHERE session_id=?1 AND seq>?2 ORDER BY seq",
            )?;
            stmt.query_map(params![session_id, after_seq], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (seq, data) = row?;
                Ok((seq, Self::message_from_data(conn, &data)?))
            })
            .collect()
        })
    }

    /// Preserve attachment metadata and original bytes in the canonical record.
    pub fn store_attachment(
        &self,
        session_id: &str,
        attachment_id: &str,
        metadata: &str,
        bytes: &[u8],
    ) -> Result<String> {
        if let Some(store) = self.scoped(session_id)? {
            return store.store_attachment(session_id, attachment_id, metadata, bytes);
        }
        anyhow::ensure!(
            !attachment_id.starts_with(CHECKPOINT_PREFIX),
            "Checkpoint IDs are records of write_checkpoint, not attachments"
        );
        self.write(|tx| {
            let hash = Self::blob_put(tx, bytes)?;
            Self::insert_attachment(tx, session_id, attachment_id, metadata, &hash)?;
            Ok(hash)
        })
    }

    /// Insert one attachment row that names `hash`. An existing row must match
    /// exactly, since attachment originals are immutable.
    fn insert_attachment(
        conn: &Connection,
        session_id: &str,
        attachment_id: &str,
        metadata: &str,
        hash: &str,
    ) -> Result<()> {
        conn.execute("INSERT INTO attachments(session_id,id,metadata,blob_hash) VALUES(?1,?2,?3,?4) ON CONFLICT(session_id,id) DO NOTHING", params![session_id, attachment_id, metadata, hash])?;
        let existing: (String, String) = conn.query_row(
            "SELECT metadata,blob_hash FROM attachments WHERE session_id=?1 AND id=?2",
            params![session_id, attachment_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        anyhow::ensure!(
            existing == (metadata.to_owned(), hash.to_owned()),
            "Attachment originals are immutable"
        );
        Ok(())
    }

    /// Key of a session's checkpoint high-water mark in `metadata`.
    fn checkpoint_high_water_key(session_id: &str) -> String {
        format!("checkpoint_high_water:{session_id}")
    }

    /// Highest checkpoint patch index this session has written (0 if none). It
    /// survives pruning, so a later checkpoint never reuses an index.
    pub fn checkpoint_high_water(&self, session_id: &str) -> Result<u64> {
        Ok(self
            .chat_metadata(session_id, &Self::checkpoint_high_water_key(session_id))?
            .map(|value| value.parse::<u64>())
            .transpose()?
            .unwrap_or(0))
    }

    /// Write one checkpoint in one write transaction. Its preimage goes to
    /// `checkpoint_blobs` and its record to `checkpoint_records`. Each `(session,
    /// keep_from)` in `prunes` first deletes that session's records whose boundary
    /// sequence is below `keep_from`, judged on the rows as they are inside this
    /// transaction; callers pass a bound, never record IDs. Preimages that no record
    /// still names are then dropped. The record's patch index must not be below the
    /// session's high-water mark, which then rises to it. Returns the preimage hash.
    /// Sessions in `prunes` must belong to this chat.
    pub fn write_checkpoint(
        &self,
        session_id: &str,
        record: CheckpointRecord<'_>,
        bytes: &[u8],
        prunes: &[(String, i64)],
    ) -> Result<String> {
        if let Some(store) = self.scoped(session_id)? {
            return store.write_checkpoint(session_id, record, bytes, prunes);
        }
        self.write(|tx| {
            let key = Self::checkpoint_high_water_key(session_id);
            let high_water = tx
                .query_row("SELECT value FROM metadata WHERE key=?1", [&key], |row| {
                    row.get::<_, String>(0)
                })
                .optional()?
                .map(|value| value.parse::<u64>())
                .transpose()?
                .unwrap_or(0);
            anyhow::ensure!(
                record.patch_index >= high_water,
                "Checkpoint patch {} is below the session high-water mark {high_water}",
                record.patch_index
            );
            let mut candidates = std::collections::BTreeSet::new();
            for (pruned_session, keep_from) in prunes {
                let stale: Vec<(String, Option<String>)> = {
                    let mut stmt = tx.prepare(
                        "SELECT id,preimage_hash FROM checkpoint_records WHERE session_id=?1 AND boundary_seq<?2",
                    )?;
                    stmt.query_map(params![pruned_session, keep_from], |row| {
                        Ok((row.get(0)?, row.get(1)?))
                    })?
                    .collect::<rusqlite::Result<_>>()?
                };
                for (id, preimage) in stale {
                    tx.execute(
                        "DELETE FROM checkpoint_records WHERE session_id=?1 AND id=?2",
                        params![pruned_session, id],
                    )?;
                    // An unrestorable record has no preimage to release.
                    candidates.extend(preimage);
                }
            }
            let hash = Self::checkpoint_put(tx, bytes)?;
            tx.execute(
                "INSERT INTO checkpoint_records(session_id,id,boundary_seq,metadata,preimage_hash) VALUES(?1,?2,?3,?4,?5)",
                params![session_id, record.id, record.boundary_seq, record.metadata, hash],
            )?;
            tx.execute(
                "INSERT OR REPLACE INTO metadata VALUES(?1,?2)",
                params![key, record.patch_index.to_string()],
            )?;
            self.drop_unreferenced_checkpoint_blobs(tx, candidates)?;
            Ok(hash)
        })
    }

    /// One-time sweep, run by the host when a chat opens. `keep` gives each session
    /// of the chat its keep bound, as `write_checkpoint` reads it; a session not
    /// listed keeps all its legacy rows. Each legacy row is an attachment under the
    /// checkpoint prefix, as an earlier build wrote it. Row by row, in one transaction:
    /// - A row at or above its bound is copied into `checkpoint_records`. Its preimage
    ///   is copied from `blobs` into `checkpoint_blobs`. A created file has no
    ///   preimage, so a missing blob is filled with empty bytes.
    /// - A row at or above its bound whose preimage is needed but missing is copied
    ///   as an `unrestorable` record, marked in its metadata. Rewind refuses any turn
    ///   that contains it.
    /// - A row below its bound is deleted, unless it needs a missing preimage. Then it
    ///   is left in `attachments` and returned.
    /// - A row whose metadata is not a JSON object, or has no boundary sequence, is
    ///   left in `attachments` and returned. Its patch index still raises the
    ///   high-water mark when readable. Its turn is unknown, so its session gets one
    ///   unrestorable record from `unreadable`, when that session has such a row.
    ///
    /// `unreadable` gives (session, boundary sequence, metadata) for the record of each
    /// session that can have one. Each session's high-water mark rises to its highest
    /// legacy patch index, and `marker` is set. Only SQLite errors fail the sweep,
    /// which rolls back. `blobs` is never changed.
    pub fn sweep_legacy_checkpoints(
        &self,
        session_id: &str,
        marker: (&str, &str),
        keep: &[(String, i64)],
        unreadable: &[(String, i64, String)],
    ) -> Result<Vec<LegacyCheckpointNote>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.sweep_legacy_checkpoints(session_id, marker, keep, unreadable);
        }
        self.write(|tx| {
            let keep: std::collections::BTreeMap<&str, i64> = keep
                .iter()
                .map(|(session, from)| (session.as_str(), *from))
                .collect();
            let legacy: Vec<(String, String, String, String)> = {
                let mut stmt = tx.prepare(
                    "SELECT session_id,id,metadata,blob_hash FROM attachments WHERE substr(id,1,length(?1))=?1 ORDER BY session_id,id",
                )?;
                stmt.query_map([CHECKPOINT_PREFIX], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<rusqlite::Result<_>>()?
            };
            let mut high_water: std::collections::BTreeMap<String, u64> = Default::default();
            let mut handled: Vec<(&str, &str)> = Vec::new();
            // Sessions with a row whose turn cannot be read. Each gets one unrestorable
            // record below, so rewinding its latest turn refuses.
            let mut unreadable_sessions: std::collections::BTreeSet<String> = Default::default();
            let mut notes = Vec::new();
            for (session, id, metadata, hash) in &legacy {
                let note = |left: bool, reason: &str| LegacyCheckpointNote {
                    session_id: session.clone(),
                    id: id.clone(),
                    reason: reason.to_owned(),
                    left,
                };
                let value = serde_json::from_str::<serde_json::Value>(metadata)
                    .ok()
                    .filter(serde_json::Value::is_object);
                if let Some(patch) = value
                    .as_ref()
                    .and_then(|value| value.get("patch_index"))
                    .and_then(serde_json::Value::as_u64)
                {
                    let highest = high_water.entry(session.clone()).or_default();
                    *highest = (*highest).max(patch);
                }
                let Some(value) = value else {
                    unreadable_sessions.insert(session.clone());
                    notes.push(note(true, "metadata is not a JSON object"));
                    continue;
                };
                let Some(boundary) = value.get("boundary_seq").and_then(serde_json::Value::as_i64)
                else {
                    unreadable_sessions.insert(session.clone());
                    notes.push(note(true, "metadata has no boundary sequence"));
                    continue;
                };
                let from = keep.get(session.as_str()).copied().unwrap_or(i64::MIN);
                let in_range = boundary >= from;
                // A created file has no preimage: restore deletes it and never reads the
                // blob. Its legacy blob is empty, so a missing one can be recreated.
                let created = matches!(
                    value.get("pre_hash"),
                    None | Some(serde_json::Value::Null)
                );
                let blob_present: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM blobs WHERE hash=?1)",
                    [hash],
                    |row| row.get(0),
                )?;
                if !created && !blob_present {
                    if !in_range {
                        notes.push(note(true, "preimage blob is missing outside the kept range"));
                        continue;
                    }
                    // An unrestorable record references no preimage blob.
                    let mut marked = value.clone();
                    marked["unrestorable"] = serde_json::json!("preimage blob is missing");
                    tx.execute(
                        "INSERT INTO checkpoint_records(session_id,id,boundary_seq,metadata,preimage_hash) VALUES(?1,?2,?3,?4,NULL)",
                        params![session, id, boundary, marked.to_string()],
                    )?;
                    notes.push(note(
                        false,
                        "preimage blob is missing; migrated as unrestorable",
                    ));
                } else if in_range {
                    // The preimage is written through the checkpoint write path, so it is
                    // encoded exactly as a new checkpoint's preimage is. A created file's
                    // missing blob becomes the empty preimage, which later checkpoints of
                    // an existing empty file reuse.
                    let preimage = if blob_present {
                        tx.execute(
                            "INSERT OR IGNORE INTO checkpoint_blobs(hash,raw_bytes,data) SELECT hash,raw_bytes,data FROM blobs WHERE hash=?1",
                            [hash],
                        )?;
                        hash.clone()
                    } else {
                        Self::checkpoint_put(tx, &[])?
                    };
                    tx.execute(
                        "INSERT INTO checkpoint_records(session_id,id,boundary_seq,metadata,preimage_hash) VALUES(?1,?2,?3,?4,?5)",
                        params![session, id, boundary, metadata, preimage],
                    )?;
                }
                handled.push((session.as_str(), id.as_str()));
            }
            for (session, id) in handled {
                tx.execute(
                    "DELETE FROM attachments WHERE session_id=?1 AND id=?2",
                    params![session, id],
                )?;
            }
            // The unreadable row's turn is unknown, so the record sits at the session's
            // latest turn start: rewinding that turn refuses. It is pruned once two newer
            // turns exist.
            for (session, boundary, metadata) in unreadable {
                if unreadable_sessions.contains(session) {
                    tx.execute(
                        "INSERT OR REPLACE INTO checkpoint_records(session_id,id,boundary_seq,metadata,preimage_hash) VALUES(?1,?2,?3,?4,NULL)",
                        params![session, UNREADABLE_RECORD_ID, boundary, metadata],
                    )?;
                }
            }
            for (session, patch) in high_water {
                let key = Self::checkpoint_high_water_key(&session);
                let current = tx
                    .query_row("SELECT value FROM metadata WHERE key=?1", [&key], |row| {
                        row.get::<_, String>(0)
                    })
                    .optional()?
                    .map(|value| value.parse::<u64>())
                    .transpose()?
                    .unwrap_or(0);
                if patch > current {
                    tx.execute(
                        "INSERT OR REPLACE INTO metadata VALUES(?1,?2)",
                        params![key, patch.to_string()],
                    )?;
                }
            }
            tx.execute(
                "INSERT OR REPLACE INTO metadata VALUES(?1,?2)",
                params![marker.0, marker.1],
            )?;
            Ok(notes)
        })
    }

    /// One page of a session's checkpoint records as (id, metadata), in id order
    /// after `after`.
    pub fn checkpoint_records_page(
        &self,
        session_id: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.checkpoint_records_page(session_id, after, limit);
        }
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id,metadata FROM checkpoint_records WHERE session_id=?1 AND (?2 IS NULL OR id>?2) ORDER BY id LIMIT ?3",
            )?;
            Ok(stmt
                .query_map(params![session_id, after, limit as i64], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Preimage bytes of one checkpoint record, or None when no such record exists.
    pub fn checkpoint_preimage(
        &self,
        session_id: &str,
        record_id: &str,
    ) -> Result<Option<Vec<u8>>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.checkpoint_preimage(session_id, record_id);
        }
        self.read(|conn| {
            // A missing record and an unrestorable record (NULL preimage) both give None.
            let hash: Option<Option<String>> = conn
                .query_row(
                    "SELECT preimage_hash FROM checkpoint_records WHERE session_id=?1 AND id=?2",
                    params![session_id, record_id],
                    |row| row.get(0),
                )
                .optional()?;
            hash.flatten()
                .map(|hash| Self::checkpoint_get(conn, &hash))
                .transpose()
        })
    }

    /// Session IDs of the chat that holds `session_id`, in order.
    pub fn chat_sessions(&self, session_id: &str) -> Result<Vec<String>> {
        match self.scoped(session_id)? {
            Some(chat) => chat.chat_session_ids(),
            None => self.chat_session_ids(),
        }
    }

    /// Session IDs of this chat store, in order. Only a chat store has them.
    pub fn chat_session_ids(&self) -> Result<Vec<String>> {
        anyhow::ensure!(
            self.0.layout.is_none(),
            "Only a chat store lists its sessions"
        );
        self.read(|conn| {
            let mut stmt = conn.prepare("SELECT id FROM sessions ORDER BY id")?;
            Ok(stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn attachment(
        &self,
        session_id: &str,
        attachment_id: &str,
    ) -> Result<Option<(String, Vec<u8>)>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.attachment(session_id, attachment_id);
        }
        self.read(|conn| {
            let row: Option<(String, String)> = conn
                .query_row(
                    "SELECT metadata,blob_hash FROM attachments WHERE session_id=?1 AND id=?2",
                    params![session_id, attachment_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            row.map(|(metadata, hash)| Ok((metadata, Self::blob_get(conn, &hash)?)))
                .transpose()
        })
    }

    /// Import an already parsed history in one application transaction.
    pub fn append_batch(&self, messages: &[Message]) -> Result<()> {
        if self.0.layout.is_some() {
            return self.layout_append_batch(messages);
        }
        if messages.is_empty() {
            return Ok(());
        }
        self.write(|tx| {
            for message in messages {
                Self::insert_message(tx, message)?;
            }
            Ok(())
        })
    }

    /// Reclaim the idle WAL while retaining FULL synchronization for writes.
    pub fn idle_checkpoint(&self) -> Result<()> {
        if self.0.layout.is_some() {
            return self.layout_checkpoint();
        }
        self.read(|conn| {
            let (busy, _, _): (i64, i64, i64) =
                conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?;
            anyhow::ensure!(busy == 0, "WAL checkpoint is busy");
            Ok(())
        })
    }

    pub(crate) fn blob_put(conn: &Connection, bytes: &[u8]) -> Result<String> {
        Self::put_blob(conn, "blobs", bytes)
    }

    pub(crate) fn blob_get(conn: &Connection, hash: &str) -> Result<Vec<u8>> {
        Self::get_blob(conn, "blobs", hash)
    }

    /// Store checkpoint preimage bytes under their plain SHA-256 in `checkpoint_blobs`.
    fn checkpoint_put(conn: &Connection, bytes: &[u8]) -> Result<String> {
        Self::put_blob(conn, "checkpoint_blobs", bytes)
    }

    fn checkpoint_get(conn: &Connection, hash: &str) -> Result<Vec<u8>> {
        Self::get_blob(conn, "checkpoint_blobs", hash)
    }

    /// Store `bytes` in `table` (a fixed table name, never user input) under their
    /// plain SHA-256, and return that hash. An existing row is kept as it is.
    fn put_blob(conn: &Connection, table: &str, bytes: &[u8]) -> Result<String> {
        let hash = format!("{:x}", Sha256::digest(bytes));
        let exists: bool = conn.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE hash=?1)"),
            [&hash],
            |row| row.get(0),
        )?;
        if !exists {
            let data = zstd::stream::encode_all(bytes, 3)?;
            conn.execute(
                &format!("INSERT INTO {table}(hash,raw_bytes,data) VALUES(?1,?2,?3)"),
                params![hash, bytes.len() as i64, data],
            )?;
        }
        Ok(hash)
    }

    fn get_blob(conn: &Connection, table: &str, hash: &str) -> Result<Vec<u8>> {
        let (length, compressed): (i64, Vec<u8>) = conn.query_row(
            &format!("SELECT raw_bytes,data FROM {table} WHERE hash=?1"),
            [hash],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let bytes = zstd::stream::decode_all(compressed.as_slice())?;
        anyhow::ensure!(
            bytes.len() as i64 == length && format!("{:x}", Sha256::digest(&bytes)) == hash,
            "Content blob integrity mismatch"
        );
        Ok(bytes)
    }

    fn attachment_content_parts(
        conn: &Connection,
        content: &str,
    ) -> Result<Option<Vec<ContentPart>>> {
        // Host envelopes put their delimiters on separate lines. JSON strings
        // escape newlines, so literal tag text inside a file cannot impersonate
        // either boundary. Retain the old parser for legacy compact envelopes.
        let boundaries = content.rfind("<BubblePawAttachments>\n").and_then(|start| {
            let body_start = start + "<BubblePawAttachments>\n".len();
            content[body_start..]
                .rfind("\n</BubblePawAttachments>")
                .map(|end| (start, body_start, body_start + end))
        });
        let (start, body_start, end) = if let Some(boundaries) = boundaries {
            boundaries
        } else {
            let Some(start) = content.rfind("<BubblePawAttachments>") else {
                return Ok(None);
            };
            let Some(end) = content[start..].find("</BubblePawAttachments>") else {
                return Ok(None);
            };
            (start, start + "<BubblePawAttachments>".len(), start + end)
        };
        let envelope = &content[body_start..end];
        let Ok(value) = serde_json::from_str::<serde_json::Value>(envelope.trim()) else {
            return Ok(None);
        };
        let Some(files) = value.get("files").and_then(|files| files.as_array()) else {
            return Ok(None);
        };
        let mut matches = Vec::new();
        for file in files {
            if let Some(inline) = file
                .get("inline")
                .and_then(|inline| inline.as_str())
                .filter(|inline| inline.len() > 4096)
            {
                let hash = Self::blob_put(conn, inline.as_bytes())?;
                let escaped = serde_json::to_string(inline)?;
                // Match the quoted value including delimiters, but retain the quotes
                // as literals so decoding needs only JSON-escaped inner bytes.
                for (position, _) in content[start..end].match_indices(&escaped) {
                    matches.push((
                        start + position + 1,
                        start + position + escaped.len() - 1,
                        hash.clone(),
                    ));
                }
            }
        }
        if matches.is_empty() {
            return Ok(None);
        }
        matches.sort_by_key(|(start, _, _)| *start);
        let mut offset = 0;
        let mut parts = Vec::new();
        for (start, end, hash) in matches {
            if start < offset {
                continue;
            }
            if start > offset {
                parts.push(ContentPart::Literal {
                    text: content[offset..start].into(),
                });
            }
            parts.push(ContentPart::Blob {
                hash,
                json_escape: true,
            });
            offset = end;
        }
        if offset < content.len() {
            parts.push(ContentPart::Literal {
                text: content[offset..].into(),
            });
        }
        Ok(Some(parts))
    }

    fn input_content_ref(conn: &Connection, text: &str) -> Result<(String, &'static str)> {
        if let Some(parts) = Self::attachment_content_parts(conn, text)? {
            Ok((Self::blob_put(conn, &serde_json::to_vec(&parts)?)?, "parts"))
        } else {
            Ok((Self::blob_put(conn, text.as_bytes())?, "text"))
        }
    }

    pub(crate) fn input_text(
        conn: &Connection,
        text: &str,
        content_blob: Option<&str>,
        content_format: Option<&str>,
    ) -> Result<String> {
        let Some(hash) = content_blob else {
            return Ok(text.to_owned());
        };
        let bytes = Self::blob_get(conn, hash)?;
        match content_format.unwrap_or("text") {
            "text" => Ok(String::from_utf8(bytes)?),
            "parts" => Self::render_content_parts(conn, serde_json::from_slice(&bytes)?),
            _ => bail!("Unknown accepted input content encoding"),
        }
    }

    fn render_content_parts(conn: &Connection, parts: Vec<ContentPart>) -> Result<String> {
        let mut content = String::new();
        for part in parts {
            match part {
                ContentPart::Literal { text } => content.push_str(&text),
                ContentPart::Blob { hash, json_escape } => {
                    let bytes = String::from_utf8(Self::blob_get(conn, &hash)?)?;
                    if json_escape {
                        let escaped = serde_json::to_string(&bytes)?;
                        content.push_str(&escaped[1..escaped.len() - 1]);
                    } else {
                        content.push_str(&bytes);
                    }
                }
            }
        }
        Ok(content)
    }

    /// The sole archive message-body decoder. Callers that only need metadata
    /// use this boundary without hydrating large content blobs.
    pub(crate) fn decode_message_body(conn: &Connection, data: &str) -> Result<serde_json::Value> {
        cold::decode_body(conn, data)
    }
    pub(crate) fn message_from_data(conn: &Connection, data: &str) -> Result<Message> {
        let mut value = Self::decode_message_body(conn, data)?;
        if let Some(hash) = value
            .get("_provider_state_blob")
            .and_then(|hash| hash.as_str())
        {
            value["provider_state"] = serde_json::from_slice(&Self::blob_get(conn, hash)?)?;
        }
        if let Some(hash) = value.get("_content_blob").and_then(|hash| hash.as_str()) {
            value["content"] =
                serde_json::Value::String(String::from_utf8(Self::blob_get(conn, hash)?)?);
        }
        if let Some(hash) = value
            .get("_content_parts_blob")
            .and_then(|value| value.as_str())
        {
            value["content"] =
                serde_json::Value::String(Self::input_text(conn, "", Some(hash), Some("parts"))?);
        } else if let Some(parts) = value.get("_content_parts") {
            value["content"] = serde_json::Value::String(Self::render_content_parts(
                conn,
                serde_json::from_value(parts.clone())?,
            )?);
        }
        Ok(serde_json::from_value(value)?)
    }

    pub(crate) fn read_message(
        conn: &Connection,
        session_id: &str,
        message_id: &str,
    ) -> Result<Message> {
        let data: String = conn.query_row(
            "SELECT data FROM messages WHERE session_id=?1 AND message_id=?2",
            params![session_id, message_id],
            |row| row.get(0),
        )?;
        Self::message_from_data(conn, &data)
    }

    /// Read the latest serving checkpoint without opening a write transaction.
    pub fn context_checkpoint(&self, session_id: &str) -> Result<Option<ContextCheckpoint>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.context_checkpoint(session_id);
        }
        let conn = self.0.connection.lock().unwrap();
        let value: Option<String> = conn
            .query_row(
                "SELECT data FROM context_checkpoints WHERE session_id=?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        value
            .map(|value| Self::decode_checkpoint(&conn, &self.hydrate_context_telemetry(&value)?))
            .transpose()
    }

    /// Read immutable chapters in session order without a write transaction.
    pub fn context_chapters(&self, session_id: &str) -> Result<Vec<ContextChapter>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.context_chapters(session_id);
        }
        let conn = self.0.connection.lock().unwrap();
        let mut statement =
            conn.prepare("SELECT data FROM context_chapters WHERE session_id=?1 ORDER BY ordinal")?;
        statement
            .query_map([session_id], |row| row.get::<_, String>(0))?
            .map(|value| {
                Ok(serde_json::from_str(
                    &self.hydrate_context_telemetry(&value?)?,
                )?)
            })
            .collect()
    }

    pub fn context_episodes(&self, session_id: &str) -> Result<Vec<ContextEpisode>> {
        self.context_records("context_episodes", "ordinal", session_id)
    }

    /// Read the frozen view without starting a write transaction.
    pub fn context_view(&self, session_id: &str) -> Result<Option<ContextView>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.context_view(session_id);
        }
        if let Some(host) = self.host() {
            let data: Option<String> = host.read(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT data FROM context_views WHERE session_id=?1",
                        [session_id],
                        |row| row.get(0),
                    )
                    .optional()?)
            })?;
            return data
                .map(|data| self.read(|conn| Self::decode_context_view(conn, &data)))
                .transpose();
        }
        let conn = self.0.connection.lock().unwrap();
        let data: Option<String> = conn
            .query_row(
                "SELECT data FROM context_views WHERE session_id=?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        data.map(|data| Self::decode_context_view(&conn, &data))
            .transpose()
    }

    fn decode_checkpoint(conn: &Connection, data: &str) -> Result<ContextCheckpoint> {
        let mut value: serde_json::Value = serde_json::from_str(data)?;
        if let Some(hash) = value.get("_summary_blob").and_then(|value| value.as_str()) {
            value["summary"] =
                serde_json::Value::String(String::from_utf8(Self::blob_get(conn, hash)?)?);
        }
        Ok(serde_json::from_value(value)?)
    }

    fn archive_checkpoint(conn: &Connection, checkpoint: &ContextCheckpoint) -> Result<String> {
        let hash = Self::blob_put(conn, checkpoint.summary.as_bytes())?;
        conn.execute("INSERT OR IGNORE INTO context_checkpoint_versions(id,session_id,summary_hash) VALUES(?1,?2,?3)", params![checkpoint.id, checkpoint.session_id, hash])?;
        let existing: String = conn.query_row(
            "SELECT summary_hash FROM context_checkpoint_versions WHERE id=?1 AND session_id=?2",
            params![checkpoint.id, checkpoint.session_id],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            existing == hash,
            "Checkpoint IDs identify immutable summaries"
        );
        Ok(hash)
    }

    fn archive_current_checkpoint(conn: &Connection, session_id: &str) -> Result<()> {
        let data: Option<String> = conn
            .query_row(
                "SELECT data FROM context_checkpoints WHERE session_id=?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(data) = data {
            Self::archive_checkpoint(conn, &Self::decode_checkpoint(conn, &data)?)?;
        }
        Ok(())
    }

    fn view_sources(
        conn: &Connection,
        session_id: &str,
        prefix: &[Message],
    ) -> Result<ViewSources> {
        Self::archive_current_checkpoint(conn, session_id)?;
        let mut texts = Vec::new();
        for message in prefix {
            let value = serde_json::to_value(message)?;
            for text in [
                message.content.clone(),
                serde_json::to_string(&value["tool_calls"])?,
                serde_json::to_string(&value["provider_state"])?,
            ] {
                if !texts.contains(&text) {
                    texts.push(text);
                }
            }
        }
        let mut sources = ViewSources {
            matches: (0..texts.len())
                .map(|_| std::collections::BTreeMap::new())
                .collect(),
            coverage: (0..texts.len())
                .map(|_| std::collections::BTreeMap::new())
                .collect(),
            texts,
            needles: HashMap::new(),
        };
        for (index, text) in sources.texts.iter().enumerate() {
            for (start, _) in text.char_indices() {
                if let Some(key) = text.as_bytes().get(start..start + 16) {
                    sources
                        .needles
                        .entry(key.try_into().unwrap())
                        .or_default()
                        .push((index, start));
                }
            }
        }
        // Derived candidates are inspected one row at a time. Only matching
        // byte-range descriptors survive; unrelated summary bodies are dropped.
        let mut statement = conn.prepare(
            "SELECT id,summary_hash FROM context_checkpoint_versions WHERE session_id=?1",
        )?;
        for row in statement.query_map([session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
            let (id, hash) = row?;
            let text = String::from_utf8(Self::blob_get(conn, &hash)?)?;
            Self::match_view_source(
                &mut sources,
                "context_checkpoint_versions",
                &id,
                "summary",
                &text,
            );
        }
        for table in [
            "context_chapters",
            "context_episodes",
            "context_subject_chapters",
        ] {
            let mut statement =
                conn.prepare(&format!("SELECT id,data FROM {table} WHERE session_id=?1"))?;
            for row in statement.query_map([session_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })? {
                let (id, data) = row?;
                let value: serde_json::Value = serde_json::from_str(&data)?;
                for field in ["summary", "description", "title"] {
                    if let Some(text) = value
                        .get(field)
                        .and_then(|value| value.as_str())
                        .filter(|text| !text.is_empty())
                    {
                        Self::match_view_source(&mut sources, table, &id, field, text);
                    }
                }
            }
        }
        // The source's retained import summary is its own derived record,
        // including when the host embeds it beside new native summaries.
        for summary in Self::imported_history_seed_at(conn, session_id)?.1 {
            let id = summary
                .id
                .strip_prefix("import-summary:")
                .context("Invalid import summary identity")?;
            Self::match_view_source(
                &mut sources,
                "imported_derived",
                id,
                "summary",
                &summary.content,
            );
        }
        // Supported canonical provenance is an actual prefix message ID or a
        // host-rendered typed-user header: [id | date] / [id]. Older-ID-only
        // indexes do not select bodies. Anonymous custom template text remains
        // literal; it must never cause a whole-history body search.
        let mut selected = std::collections::BTreeSet::new();
        for message in prefix {
            selected.insert(message.id.as_str());
            for candidate in message.content.split('[').skip(1) {
                if let Some((header, _)) = candidate.split_once(']') {
                    let id = header.split_once(" | ").map_or(header, |(id, _)| id);
                    if !id.is_empty() {
                        selected.insert(id);
                    }
                }
            }
        }
        for message_id in selected {
            let canonical: Option<(i64, String)> = conn
                .query_row(
                    "SELECT seq,data FROM messages WHERE session_id=?1 AND message_id=?2",
                    params![session_id, message_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((seq, data)) = canonical else {
                continue;
            };
            let original = Self::message_from_data(conn, &data)?;
            let id = seq.to_string();
            Self::match_view_source(&mut sources, "messages", &id, "content", &original.content);
            let value = serde_json::to_value(original)?;
            for field in ["tool_calls", "provider_state"] {
                if let Some(value) = value.get(field) {
                    Self::match_view_source(
                        &mut sources,
                        "messages",
                        &id,
                        &format!("json:{field}"),
                        &serde_json::to_string(value)?,
                    );
                }
            }
        }
        Ok(sources)
    }

    fn cover_view_text(
        coverage: &mut std::collections::BTreeMap<usize, usize>,
        mut start: usize,
        mut end: usize,
    ) {
        if let Some((&previous, &through)) = coverage.range(..=start).next_back()
            && through >= start
        {
            start = previous;
            end = end.max(through);
            coverage.remove(&previous);
        }
        loop {
            let next = coverage
                .range(start..=end)
                .next()
                .map(|(&start, &end)| (start, end));
            let Some((next, through)) = next else {
                break;
            };
            end = end.max(through);
            coverage.remove(&next);
        }
        coverage.insert(start, end);
    }

    fn match_view_source(
        sources: &mut ViewSources,
        table: &str,
        id: &str,
        field: &str,
        text: &str,
    ) {
        if text.is_empty() {
            return;
        }
        let source = Arc::new(TextSourceRef {
            table: table.into(),
            id: id.into(),
            field: field.into(),
        });
        for (source_start, _) in text.char_indices() {
            if sources.needles.is_empty() {
                break;
            }
            let Some(key) = text.as_bytes().get(source_start..source_start + 16) else {
                break;
            };
            let key: [u8; 16] = key.try_into().unwrap();
            let Some(candidates) = sources.needles.remove(&key) else {
                continue;
            };
            let mut remaining = Vec::new();
            for (index, start) in candidates {
                // Covered positions can later be rendered using a subrange of
                // that pointer, even if another overlapping pointer wins first.
                if sources.coverage[index]
                    .range(..=start)
                    .next_back()
                    .is_some_and(|(&begin, &end)| begin < start && end > start)
                {
                    continue;
                }
                let target = &sources.texts[index][start..];
                let previous = sources.matches[index]
                    .get(&start)
                    .map_or(0, |found| found.length);
                if previous >= target.len() {
                    continue;
                }
                // A later occurrence can only improve the best range if the
                // first as-yet-unmatched byte matches. Repeated long near-matches
                // therefore avoid repeatedly comparing their whole prefix.
                if previous > 0
                    && target.as_bytes().get(previous)
                        != text.as_bytes().get(source_start + previous)
                {
                    remaining.push((index, start));
                    continue;
                }
                let mut length = target
                    .as_bytes()
                    .iter()
                    .zip(&text.as_bytes()[source_start..])
                    .take_while(|(left, right)| left == right)
                    .count();
                while !target.is_char_boundary(length) {
                    length -= 1;
                }
                if length > previous {
                    sources.matches[index].insert(
                        start,
                        ViewTextMatch {
                            source: source.clone(),
                            start: source_start,
                            length,
                        },
                    );
                    Self::cover_view_text(&mut sources.coverage[index], start, start + length);
                }
                if length.max(previous) < target.len() {
                    remaining.push((index, start));
                }
            }
            if !remaining.is_empty() {
                sources.needles.insert(key, remaining);
            }
        }
        if text.len() < 16 {
            for (index, target) in sources.texts.iter().enumerate() {
                for (start, _) in target.match_indices(text) {
                    let matches = &mut sources.matches[index];
                    if matches
                        .get(&start)
                        .is_none_or(|previous| text.len() > previous.length)
                    {
                        matches.insert(
                            start,
                            ViewTextMatch {
                                source: source.clone(),
                                start: 0,
                                length: text.len(),
                            },
                        );
                        Self::cover_view_text(
                            &mut sources.coverage[index],
                            start,
                            start + text.len(),
                        );
                    }
                }
            }
        }
    }

    fn text_pointers(text: &str, sources: &ViewSources) -> Vec<TextPointer> {
        let index = sources
            .texts
            .iter()
            .position(|target| target == text)
            .unwrap();
        let mut matches = sources.matches[index].iter().peekable();
        let mut best: Option<(usize, &ViewTextMatch)> = None;
        let mut pointers = Vec::new();
        let mut offset = 0;
        let mut literal_start = 0;
        while offset < text.len() {
            while let Some((&start, found)) = matches.peek().copied()
                && start <= offset
            {
                if best.is_none_or(|(previous, old)| start + found.length > previous + old.length) {
                    best = Some((start, found));
                }
                matches.next();
            }
            if let Some((start, found)) = best
                && start + found.length > offset
            {
                if literal_start < offset {
                    pointers.push(TextPointer::Literal {
                        text: text[literal_start..offset].into(),
                    });
                }
                pointers.push(TextPointer::Source {
                    table: found.source.table.clone(),
                    id: found.source.id.clone(),
                    field: found.source.field.clone(),
                    start: found.start + offset - start,
                    end: found.start + found.length,
                });
                offset = start + found.length;
                literal_start = offset;
            } else {
                offset += text[offset..].chars().next().unwrap().len_utf8();
            }
        }
        if literal_start < text.len() {
            pointers.push(TextPointer::Literal {
                text: text[literal_start..].into(),
            });
        }
        pointers
    }

    fn encode_context_view(conn: &Connection, view: &ContextView) -> Result<String> {
        let sources = Self::view_sources(conn, &view.session_id, &view.prefix)?;
        let mut pointers = Vec::new();
        for message in &view.prefix {
            let canonical: Option<(i64, String)> = conn
                .query_row(
                    "SELECT seq,data FROM messages WHERE session_id=?1 AND message_id=?2",
                    params![view.session_id, message.id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((seq, data)) = &canonical
                && serde_json::to_vec(&Self::message_from_data(conn, data)?)?
                    == serde_json::to_vec(message)?
            {
                // Global seq gaps from other sessions do not change this range.
                if let Some(PrefixPointer::Messages { last, .. }) = pointers.last_mut() {
                    let next: Option<i64> = conn.query_row(
                        "SELECT MIN(seq) FROM messages WHERE session_id=?1 AND seq>?2",
                        params![view.session_id, *last],
                        |row| row.get(0),
                    )?;
                    if next == Some(*seq) {
                        *last = *seq;
                        continue;
                    }
                }
                pointers.push(PrefixPointer::Messages {
                    first: *seq,
                    last: *seq,
                });
                continue;
            }
            let mut metadata = serde_json::to_value(message)?;
            let mut fields = HashMap::new();
            let original = canonical
                .as_ref()
                .map(|(_, data)| -> Result<_> {
                    Ok(serde_json::to_value(Self::message_from_data(conn, data)?)?)
                })
                .transpose()?;
            for field in ["tool_calls", "provider_state"] {
                let value = metadata
                    .as_object_mut()
                    .unwrap()
                    .remove(field)
                    .unwrap_or(serde_json::Value::Null);
                let inherited = original
                    .as_ref()
                    .and_then(|value| value.get(field))
                    .unwrap_or(&serde_json::Value::Null);
                // Value equality collapses signed zero. A frozen projection must
                // preserve the exact JSON spelling used by its provider request.
                if serde_json::to_vec(&value)? != serde_json::to_vec(inherited)? {
                    fields.insert(
                        field.to_owned(),
                        Self::text_pointers(&serde_json::to_string(&value)?, &sources),
                    );
                }
            }
            metadata.as_object_mut().unwrap().remove("content");
            pointers.push(PrefixPointer::Rendered {
                base_seq: canonical.map(|(seq, _)| seq),
                metadata,
                content: Self::text_pointers(&message.content, &sources),
                fields,
            });
        }
        let mut value = serde_json::to_value(view)?;
        value.as_object_mut().unwrap().remove("prefix");
        value["prefix_refs"] = serde_json::to_value(pointers)?;
        Ok(serde_json::to_string(&value)?)
    }

    fn decode_context_view(conn: &Connection, data: &str) -> Result<ContextView> {
        let mut value: serde_json::Value = serde_json::from_str(data)?;
        let Some(refs) = value.get("prefix_refs") else {
            return Ok(serde_json::from_value(value)?);
        };
        let pointers: Vec<PrefixPointer> = serde_json::from_value(refs.clone())?;
        let session_id = value["session_id"]
            .as_str()
            .context("View has no session")?;
        let mut prefix = Vec::new();
        for pointer in pointers {
            match pointer {
                PrefixPointer::Messages { first, last } => {
                    let mut statement = conn.prepare("SELECT data FROM messages WHERE session_id=?1 AND seq BETWEEN ?2 AND ?3 ORDER BY seq")?;
                    for row in statement.query_map(params![session_id, first, last], |row| {
                        row.get::<_, String>(0)
                    })? {
                        prefix.push(Self::message_from_data(conn, &row?)?);
                    }
                }
                PrefixPointer::Rendered {
                    base_seq,
                    metadata,
                    content,
                    fields,
                } => {
                    let mut value = if let Some(seq) = base_seq {
                        let data: String = conn.query_row(
                            "SELECT data FROM messages WHERE session_id=?1 AND seq=?2",
                            params![session_id, seq],
                            |row| row.get(0),
                        )?;
                        let mut original =
                            serde_json::to_value(Self::message_from_data(conn, &data)?)?;
                        original
                            .as_object_mut()
                            .unwrap()
                            .retain(|field, _| field == "tool_calls" || field == "provider_state");
                        original
                    } else {
                        serde_json::json!({})
                    };
                    value.as_object_mut().unwrap().extend(
                        metadata
                            .as_object()
                            .context("Invalid rendered message metadata")?
                            .clone(),
                    );
                    value["content"] = serde_json::Value::String(Self::render_text_pointers(
                        conn, session_id, content,
                    )?);
                    for (field, pointers) in fields {
                        value[&field] = serde_json::from_str(&Self::render_text_pointers(
                            conn, session_id, pointers,
                        )?)?;
                    }
                    prefix.push(serde_json::from_value(value)?);
                }
            }
        }
        value["prefix"] = serde_json::to_value(prefix)?;
        Ok(serde_json::from_value(value)?)
    }

    fn render_text_pointers(
        conn: &Connection,
        session_id: &str,
        pointers: Vec<TextPointer>,
    ) -> Result<String> {
        let mut rendered = String::new();
        for pointer in pointers {
            match pointer {
                TextPointer::Literal { text } => rendered.push_str(&text),
                TextPointer::Source {
                    table,
                    id,
                    field,
                    start,
                    end,
                } => {
                    let text = if table == "messages" {
                        let data: String = conn.query_row(
                            "SELECT data FROM messages WHERE session_id=?1 AND seq=?2",
                            params![session_id, id.parse::<i64>()?],
                            |row| row.get(0),
                        )?;
                        let message = Self::message_from_data(conn, &data)?;
                        if let Some(field) = field.strip_prefix("json:") {
                            serde_json::to_string(
                                &serde_json::to_value(message)?
                                    .get(field)
                                    .context("View message field is absent")?,
                            )?
                        } else {
                            message.content
                        }
                    } else if table == "imported_derived" {
                        anyhow::ensure!(field == "summary", "Unknown imported view source field");
                        let data: String = conn.query_row(
                            "SELECT data FROM imported_derived WHERE session_id=?1 AND id=?2",
                            params![session_id, id],
                            |row| row.get(0),
                        )?;
                        let record: crate::transcript::ImportDerivedRecord =
                            serde_json::from_str(&data)?;
                        Self::import_summary_text(&record)
                            .context("Imported summary source has no text")?
                    } else if table == "context_checkpoint_versions" {
                        let hash: String = conn.query_row("SELECT summary_hash FROM context_checkpoint_versions WHERE session_id=?1 AND id=?2", params![session_id, id], |row| row.get(0))?;
                        String::from_utf8(Self::blob_get(conn, &hash)?)?
                    } else {
                        anyhow::ensure!(
                            [
                                "context_chapters",
                                "context_episodes",
                                "context_subject_chapters"
                            ]
                            .contains(&table.as_str()),
                            "Invalid view source table"
                        );
                        let data: String = conn.query_row(
                            &format!("SELECT data FROM {table} WHERE session_id=?1 AND id=?2"),
                            params![session_id, id],
                            |row| row.get(0),
                        )?;
                        let source: serde_json::Value = serde_json::from_str(&data)?;
                        source
                            .get(&field)
                            .and_then(|field| field.as_str())
                            .context("View source text is absent")?
                            .to_owned()
                    };
                    rendered.push_str(
                        text.get(start..end)
                            .context("Invalid view text source range")?,
                    );
                }
            }
        }
        Ok(rendered)
    }

    /// Resolve a canonical message ID to its global SQLite sequence.
    pub fn message_seq(&self, session_id: &str, message_id: &str) -> Result<i64> {
        if let Some(store) = self.scoped(session_id)? {
            return store.message_seq(session_id, message_id);
        }
        let conn = self.0.connection.lock().unwrap();
        Self::session_message_seq(&conn, session_id, message_id)
    }

    /// Resolve a view's inclusive boundary back to a canonical message ID.
    pub fn context_view_frontier_message_id(
        &self,
        session_id: &str,
        frontier: Option<i64>,
    ) -> Result<Option<String>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.context_view_frontier_message_id(session_id, frontier);
        }
        let conn = self.0.connection.lock().unwrap();
        frontier
            .map(|seq| {
                conn.query_row(
                    "SELECT message_id FROM messages WHERE session_id=?1 AND seq=?2",
                    params![session_id, seq],
                    |row| row.get(0),
                )
                .optional()?
                .context("View frontier does not belong to the session")
            })
            .transpose()
    }

    /// Replace all view data in exactly one transaction after validating its references.
    pub fn save_context_view(&self, view: &ContextView) -> Result<()> {
        self.save_context_view_events(view, &[])
    }

    /// Publish a view with an authoritative apply fact in its existing validation transaction.
    pub fn save_context_view_with_event(
        &self,
        view: &ContextView,
        event: &ContextEvent,
    ) -> Result<()> {
        anyhow::ensure!(
            event.kind == ContextEventKind::Applied,
            "View requires an applied event"
        );
        self.save_context_view_events(view, std::slice::from_ref(event))
    }

    fn save_context_view_events(&self, view: &ContextView, events: &[ContextEvent]) -> Result<()> {
        if let Some(store) = self.scoped(&view.session_id)? {
            return store.save_context_view_events(view, events);
        }
        anyhow::ensure!(
            !view.session_id.trim().is_empty(),
            "View session ID is empty"
        );
        anyhow::ensure!(
            !view.created_at.trim().is_empty(),
            "View creation time is empty"
        );
        self.write_context_view_events(view, events, |tx| {
            let session = Self::session_from_connection(tx, &view.session_id)?;
            if let Some(frontier) = view.frontier {
                let member: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE session_id=?1 AND seq=?2)", params![view.session_id, frontier], |row| row.get(0))?;
                anyhow::ensure!(member, "View frontier does not belong to the session");
                if session.context_strategy == ContextStrategy::Classical {
                    // A ready summary snapshot may be applied while the worker
                    // advances the latest checkpoint. Its frozen prefix owns
                    // that snapshot's text independently of the latest pointer.
                    let covered: bool = tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM context_checkpoints WHERE session_id=?1 AND through_message_seq>=?2)",
                        params![view.session_id, frontier], |row| row.get(0),
                    )?;
                    let source_boundary = Self::imported_history_seed_at(tx,&view.session_id)?.0;
                    anyhow::ensure!(covered || source_boundary==Some(frontier), "Classical view frontier must be covered by a checkpoint or imported summary");
                    Self::validate_complete_frontier(tx, &view.session_id, frontier)?;
                } else {
                    let boundary: bool = tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM context_episodes WHERE session_id=?1 AND last_message_seq=?2) OR EXISTS(SELECT 1 FROM context_chapters WHERE session_id=?1 AND last_message_seq=?2) OR EXISTS(SELECT 1 FROM context_checkpoints WHERE session_id=?1 AND through_message_seq=?2)",
                        params![view.session_id, frontier], |row| row.get(0),
                    )?;
                    let source_boundary = Self::imported_history_seed_at(tx,&view.session_id)?.0;
                    anyhow::ensure!(boundary || source_boundary==Some(frontier), "View frontier must be an episode, legacy or imported coverage boundary");
                }
            }
            for message_id in &view.masked_tool_output_ids {
                let seq = Self::session_message_seq(tx, &view.session_id, message_id)?;
                anyhow::ensure!(view.frontier.is_none_or(|frontier| seq > frontier), "Masked output must be in the raw zone");
                let output:bool=tx.query_row("SELECT h.role='tool' AND EXISTS(SELECT 1 FROM message_tool_calls WHERE session_id=?1 AND message_seq=?2 AND kind='result') FROM message_headers h WHERE h.session_id=?1 AND h.message_seq=?2",params![view.session_id,seq],|row|row.get(0))?;
                anyhow::ensure!(output, "Masked message must be a tool output");
            }
            if !view.clipped_tool_call_ids.is_empty() {
                for call_id in &view.clipped_tool_call_ids {
                    let member:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM message_tool_calls WHERE session_id=?1 AND call_id=?2 AND message_seq>?3 AND kind='call')",params![view.session_id,call_id,view.frontier.unwrap_or(0)],|row|row.get(0))?;
                    anyhow::ensure!(member, "Clipped tool call must belong to the session raw zone");
                }
            }
            let data = Self::encode_context_view(tx, view)?;
            Self::insert_context_events_tx(tx, &view.session_id, events)?;
            Ok(data)
        })
    }

    fn session_from_connection(conn: &Connection, session_id: &str) -> Result<Session> {
        let data: Option<String> = conn
            .query_row(
                "SELECT data FROM sessions WHERE id=?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(serde_json::from_str(
            &data.context("Context session does not exist")?,
        )?)
    }

    /// A coverage boundary must not separate an assistant call from its results.
    fn validate_complete_frontier(
        conn: &Connection,
        session_id: &str,
        frontier: i64,
    ) -> Result<()> {
        // The source's own compaction closes its prior serving round. Original
        // unmatched source calls remain in history and the no-reuse registry;
        // they are not BubblePaw effects and cannot block later native summaries.
        let source = Self::imported_history_seed_at(conn, session_id)?
            .0
            .filter(|source| *source <= frontier)
            .unwrap_or(0);
        let mut statement = conn.prepare("SELECT call_id FROM message_tool_calls WHERE session_id=?1 AND message_seq>?2 AND message_seq<=?3 GROUP BY call_id HAVING MAX(CASE WHEN kind='call' THEN message_seq END)>COALESCE(MAX(CASE WHEN kind='result' THEN message_seq END),0)")?;
        let pending = statement
            .query_map(params![session_id, source, frontier], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        anyhow::ensure!(
            pending.is_empty(),
            "Context frontier must preserve complete tool rounds"
        );
        Ok(())
    }

    pub fn context_subject_chapters(&self, session_id: &str) -> Result<Vec<ContextSubjectChapter>> {
        self.context_records("context_subject_chapters", "ordinal", session_id)
    }

    pub fn context_decisions(&self, session_id: &str) -> Result<Vec<ContextDecision>> {
        self.context_records("context_decisions", "episode_ordinal,rowid", session_id)
    }

    fn context_records<T: serde::de::DeserializeOwned>(
        &self,
        table: &str,
        order: &str,
        session_id: &str,
    ) -> Result<Vec<T>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.context_records(table, order, session_id);
        }
        let conn = self.0.connection.lock().unwrap();
        let mut statement = conn.prepare(&format!(
            "SELECT data FROM {table} WHERE session_id=?1 ORDER BY {order}"
        ))?;
        statement
            .query_map([session_id], |row| row.get::<_, String>(0))?
            .map(|value| {
                Ok(serde_json::from_str(
                    &self.hydrate_context_telemetry(&value?)?,
                )?)
            })
            .collect()
    }

    pub fn open_subject_chapter(&self, session_id: &str) -> Result<Option<ContextSubjectChapter>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.open_subject_chapter(session_id);
        }
        let conn = self.0.connection.lock().unwrap();
        Self::read_open_subject_chapter(&conn, session_id)?
            .map(|chapter| {
                let data = self.hydrate_context_telemetry(&serde_json::to_string(&chapter)?)?;
                Ok(serde_json::from_str(&data)?)
            })
            .transpose()
    }

    fn read_open_subject_chapter(
        conn: &Connection,
        session_id: &str,
    ) -> Result<Option<ContextSubjectChapter>> {
        let data: Option<String> = conn
            .query_row(
                "SELECT data FROM context_subject_chapters WHERE session_id=?1 AND status='open'",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        data.map(|data| Ok(serde_json::from_str(&data)?))
            .transpose()
    }

    /// Last covered canonical message, including frozen legacy coverage.
    pub fn episode_coverage_token(&self, session_id: &str) -> Result<Option<String>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.episode_coverage_token(session_id);
        }
        let conn = self.0.connection.lock().unwrap();
        Ok(Self::episode_coverage(&conn, session_id)?.2)
    }

    fn episode_coverage(conn: &Connection, session_id: &str) -> Result<(i64, i64, Option<String>)> {
        let latest: Option<(i64, i64)> = conn
            .query_row(
                "SELECT ordinal,last_message_seq FROM context_episodes WHERE session_id=?1 ORDER BY ordinal DESC LIMIT 1",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (ordinal, last) = match latest {
            Some(latest) => latest,
            None => {
                let cutoff: i64 = conn.query_row(
                    "SELECT COALESCE((SELECT last_message_seq FROM context_chapters WHERE session_id=?1 ORDER BY ordinal DESC LIMIT 1),(SELECT through_message_seq FROM context_checkpoints WHERE session_id=?1),0)",
                    [session_id],
                    |row| row.get(0),
                )?;
                let cutoff = if cutoff == 0 {
                    Self::imported_history_seed_at(conn, session_id)?
                        .0
                        .unwrap_or(0)
                } else {
                    cutoff
                };
                (0, cutoff)
            }
        };
        let token = if last == 0 {
            None
        } else {
            Some(conn.query_row(
                "SELECT message_id FROM messages WHERE session_id=?1 AND seq=?2",
                params![session_id, last],
                |row| row.get(0),
            )?)
        };
        Ok((ordinal, last, token))
    }

    /// Append one chapter atomically. Existing chapters and original messages
    /// are never replaced. The host enforces its model-specific token limit;
    /// this boundary also bounds durable summary bytes.
    pub fn save_context_chapter(&self, chapter: &ContextChapter) -> Result<()> {
        if let Some(store) = self.scoped(&chapter.session_id)? {
            return store.save_context_chapter(chapter);
        }
        anyhow::ensure!(!chapter.id.trim().is_empty(), "Chapter ID is empty");
        anyhow::ensure!(
            !chapter.session_id.trim().is_empty(),
            "Chapter session ID is empty"
        );
        anyhow::ensure!(
            !chapter.first_message_id.trim().is_empty()
                && !chapter.last_message_id.trim().is_empty(),
            "Chapter message range is empty"
        );
        let ordinal = i64::try_from(chapter.ordinal).context("Chapter ordinal is too large")?;
        anyhow::ensure!(ordinal > 0, "Chapter ordinal must be one-based");
        anyhow::ensure!(
            !chapter.title.trim().is_empty()
                && chapter.title.chars().count() <= 80
                && !chapter.title.contains(['\r', '\n']),
            "Chapter title must be nonempty, single-line and at most 80 characters"
        );
        anyhow::ensure!(
            !chapter.summary.trim().is_empty() && chapter.summary.len() <= 64 * 1024,
            "Chapter summary must be nonempty and at most 65536 bytes"
        );
        anyhow::ensure!(!chapter.model.trim().is_empty(), "Chapter model is empty");
        anyhow::ensure!(
            !chapter.created_at.trim().is_empty(),
            "Chapter creation time is empty"
        );
        let data = serde_json::to_string(chapter)?;
        self.write_context(|tx| {
            let message_seq = |message_id: &str| -> Result<i64> {
                tx.query_row(
                    "SELECT seq FROM messages WHERE session_id=?1 AND message_id=?2",
                    params![chapter.session_id, message_id],
                    |row| row.get(0),
                )
                .optional()?
                .context("Chapter range endpoint does not belong to the session")
            };
            let first = message_seq(&chapter.first_message_id)?;
            let last = message_seq(&chapter.last_message_id)?;
            anyhow::ensure!(first <= last, "Chapter range cannot regress");
            let previous: Option<(i64, i64)> = tx
                .query_row(
                    "SELECT ordinal,last_message_seq FROM context_chapters WHERE session_id=?1 ORDER BY ordinal DESC LIMIT 1",
                    [&chapter.session_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let (previous_ordinal, previous_last) = match previous {
                Some(previous) => previous,
                None => {
                    let cutoff: Option<i64> = tx
                        .query_row(
                            "SELECT through_message_seq FROM context_checkpoints WHERE session_id=?1",
                            [&chapter.session_id],
                            |row| row.get(0),
                        )
                        .optional()?;
                    (0, cutoff.unwrap_or(0))
                }
            };
            anyhow::ensure!(
                previous_ordinal.checked_add(1) == Some(ordinal),
                "Chapter ordinal must follow the previous chapter"
            );
            let next: Option<i64> = tx.query_row(
                "SELECT MIN(seq) FROM messages WHERE session_id=?1 AND seq>?2",
                params![chapter.session_id, previous_last],
                |row| row.get(0),
            )?;
            anyhow::ensure!(
                next == Some(first),
                "Chapter range must start at the next session message after the previous chapter or legacy cutoff"
            );
            tx.execute(
                "INSERT INTO context_chapters(session_id,id,ordinal,first_message_seq,last_message_seq,data) VALUES(?1,?2,?3,?4,?5,?6)",
                params![chapter.session_id, chapter.id, ordinal, first, last, data],
            )?;
            Ok(())
        })
    }

    /// Replace the session's checkpoint atomically, without deleting or copying history.
    pub fn save_context_checkpoint(&self, checkpoint: &ContextCheckpoint) -> Result<()> {
        if let Some(store) = self.scoped(&checkpoint.session_id)? {
            return store.save_context_checkpoint(checkpoint);
        }
        anyhow::ensure!(!checkpoint.id.trim().is_empty(), "Checkpoint ID is empty");
        anyhow::ensure!(
            !checkpoint.session_id.trim().is_empty(),
            "Checkpoint session ID is empty"
        );
        anyhow::ensure!(
            !checkpoint.through_message_id.trim().is_empty(),
            "Checkpoint cutoff is empty"
        );
        anyhow::ensure!(
            !checkpoint.model.trim().is_empty(),
            "Checkpoint model is empty"
        );
        anyhow::ensure!(
            !checkpoint.summary.trim().is_empty() && checkpoint.summary.len() <= 64 * 1024,
            "Checkpoint summary must be nonempty and at most 65536 bytes"
        );
        self.write_context(|tx| {
            let cutoff: Option<i64> = tx.query_row(
                "SELECT seq FROM messages WHERE session_id=?1 AND message_id=?2",
                params![checkpoint.session_id, checkpoint.through_message_id],
                |row| row.get(0),
            ).optional()?;
            let cutoff = cutoff.context("Checkpoint cutoff does not belong to the session")?;
            let session = Self::session_from_connection(tx, &checkpoint.session_id)?;
            if session.context_strategy == ContextStrategy::Classical {
                Self::validate_complete_frontier(tx, &checkpoint.session_id, cutoff)?;
            }
            let previous: Option<i64> = tx.query_row(
                "SELECT through_message_seq FROM context_checkpoints WHERE session_id=?1",
                [&checkpoint.session_id],
                |row| row.get(0),
            ).optional()?;
            anyhow::ensure!(previous.is_none_or(|previous| cutoff >= previous), "Checkpoint cutoff cannot regress");
            // Chapters and episodes start after the legacy cutoff, freezing it.
            let chapters: i64 = tx.query_row(
                "SELECT (SELECT COUNT(*) FROM context_chapters WHERE session_id=?1)+(SELECT COUNT(*) FROM context_episodes WHERE session_id=?1)",
                [&checkpoint.session_id],
                |row| row.get(0),
            )?;
            anyhow::ensure!(chapters == 0, "Legacy checkpoint is frozen once context chapters or episodes exist");
            // Preserve the previous immutable summary before replacing the latest pointer.
            Self::archive_current_checkpoint(tx, &checkpoint.session_id)?;
            let hash = Self::archive_checkpoint(tx, checkpoint)?;
            let mut data = serde_json::to_value(checkpoint)?;
            data["summary"] = serde_json::Value::String(String::new());
            data["_summary_blob"] = serde_json::Value::String(hash);
            let data = serde_json::to_string(&data)?;
            tx.execute(
                "INSERT INTO context_checkpoints(session_id,id,through_message_seq,data) VALUES(?1,?2,?3,?4) ON CONFLICT(session_id) DO UPDATE SET id=excluded.id,through_message_seq=excluded.through_message_seq,data=excluded.data",
                params![checkpoint.session_id, checkpoint.id, cutoff, data],
            )?;
            Ok(())
        })
    }
    pub(crate) fn run_from_data(conn: &Connection, data: &str) -> Result<Run> {
        let mut value: serde_json::Value = serde_json::from_str(data)?;
        if let Some(hash) = value.get("_error_blob").and_then(|hash| hash.as_str()) {
            value["error"] =
                serde_json::Value::String(String::from_utf8(Self::blob_get(conn, hash)?)?);
        }
        Ok(serde_json::from_value(value)?)
    }

    fn run_metadata(conn: &Connection, run: &Run) -> Result<String> {
        let mut value = serde_json::to_value(run)?;
        value["text"] = serde_json::Value::Null;
        if let Some(error) = &run.error {
            let hash = Self::blob_put(conn, error.as_bytes())?;
            value["error"] = serde_json::Value::Null;
            value["_error_blob"] = serde_json::Value::String(hash);
        }
        Ok(serde_json::to_string(&value)?)
    }

    pub fn run(&self, run_id: &str) -> Result<Run> {
        if self.0.layout.is_some() {
            return self.find_record("runs", run_id)?.run(run_id);
        }
        let (value, _) = self.run_row(run_id)?;
        self.read(|conn| Self::run_from_data(conn, &value))
    }
    pub(crate) fn hydrate_run(&self, mut run: Run) -> Result<Run> {
        if let Some(store) = self.scoped(&run.session_id)? {
            return store.hydrate_run(run);
        }
        if run.text.is_some() {
            return Ok(run);
        }
        {
            let (_, result) = self.run_row(&run.id)?;
            let conn = self.0.connection.lock().unwrap();
            if let Some(message_id) = result {
                run.text = Some(Self::read_message(&conn, &run.session_id, &message_id)?.content);
                return Ok(run);
            }
        }
        // Results belong to completed messages, not the idle runtime's metadata cache.
        if run.status == "completed" {
            let start = chrono::DateTime::parse_from_rfc3339(&run.started_at)?;
            let end = chrono::DateTime::parse_from_rfc3339(
                run.ended_at
                    .as_deref()
                    .context("Completed run has no end time")?,
            )?;
            let conn = self.0.connection.lock().unwrap();
            let mut statement =
                conn.prepare("SELECT data FROM messages WHERE session_id=?1 ORDER BY seq DESC")?;
            let mut rows = statement.query([&run.session_id])?;
            while let Some(row) = rows.next()? {
                let message = Self::message_from_data(&conn, &row.get::<_, String>(0)?)?;
                let created = chrono::DateTime::parse_from_rfc3339(&message.created_at)?;
                if created < start {
                    break;
                }
                // A script run ends on its tool result, not on an assistant message.
                let result = message.role == Role::Assistant
                    || (run.model == crate::SCRIPT_MODEL && message.role == Role::Tool);
                if created <= end && result {
                    run.text = Some(message.content);
                    break;
                }
            }
        }
        Ok(run)
    }
    pub fn runs(&self, session_id: &str) -> Result<Vec<Run>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.all_runs(session_id);
        }
        self.all_runs(session_id)
    }
    #[cfg(test)]
    pub(crate) fn start_run(&self, run: &Run) -> Result<()> {
        if let Some(store) = self.scoped(&run.session_id)? {
            return store.start_run(run);
        }
        self.write(|tx| {
            tx.execute(
                "INSERT INTO runs(id,session_id,status,data) VALUES(?1,?2,'running',?3)",
                params![run.id, run.session_id, serde_json::to_string(run)?],
            )?;
            Ok(())
        })
    }
    pub(crate) fn finish_run(&self, run: &Run) -> Result<()> {
        if let Some(store) = self.scoped(&run.session_id)? {
            return store.finish_run(run);
        }
        self.write(|tx| {
            tx.execute(
                "UPDATE runs SET status=?2,data=?3 WHERE id=?1",
                params![run.id, run.status, Self::run_metadata(tx, run)?],
            )?;
            Ok(())
        })
    }
    pub(crate) fn message(session_id: &str, role: Role, content: String) -> Message {
        Message {
            id: id(),
            session_id: session_id.into(),
            role,
            origin: None,
            content,
            tool_calls: Vec::new(),
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: now(),
        }
    }
    pub(crate) fn insert_message(tx: &Connection, message: &Message) -> Result<()> {
        Self::insert_message_with_blobs(tx, message, false)
    }

    fn insert_message_with_blobs(
        tx: &Connection,
        message: &Message,
        blob_large_all_roles: bool,
    ) -> Result<()> {
        let mut data = serde_json::to_value(message)?;
        if let Some(state) = &message.provider_state {
            let opaque = serde_json::to_vec(state)?;
            if opaque.len() > 4096 {
                let hash = Self::blob_put(tx, &opaque)?;
                data.as_object_mut().unwrap().remove("provider_state");
                data["_provider_state_blob"] = serde_json::Value::String(hash);
            }
        }
        // Failed script runs expose this same body as Run.error. Blob-back
        // even a small error so the terminal record can share its exact hash.
        if let Some(parts) = Self::attachment_content_parts(tx, &message.content)? {
            data["content"] = serde_json::Value::String(String::new());
            let hash = Self::blob_put(tx, &serde_json::to_vec(&parts)?)?;
            data["_content_parts_blob"] = serde_json::Value::String(hash);
        } else if (message.role == Role::Tool && message.is_error)
            || (message.content.len() > 4096
                && (blob_large_all_roles
                    || message.role == Role::Tool
                    || message.content.contains("<BubblePawAttachments>")))
        {
            let hash = Self::blob_put(tx, message.content.as_bytes())?;
            data["content"] = serde_json::Value::String(String::new());
            data["_content_blob"] = serde_json::Value::String(hash);
        } else if message.role == Role::User {
            let hash = format!("{:x}", Sha256::digest(message.content.as_bytes()));
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM blobs WHERE hash=?1)",
                [&hash],
                |row| row.get(0),
            )?;
            if exists {
                data["content"] = serde_json::Value::String(String::new());
                data["_content_blob"] = serde_json::Value::String(hash);
            }
        }
        tx.execute(
            "INSERT INTO messages(session_id,message_id,data) VALUES(?1,?2,?3)",
            params![
                message.session_id,
                message.id,
                serde_json::to_string(&data)?
            ],
        )?;
        let seq = tx.last_insert_rowid();
        Self::insert_message_header(tx, seq, message)?;
        for call in &message.tool_calls {
            tx.execute(
                "INSERT OR IGNORE INTO message_tool_calls VALUES(?1,?2,?3,'call')",
                params![message.session_id, call.id, seq],
            )?;
        }
        if message.role == Role::Tool
            && let Some(call) = &message.tool_call_id
        {
            tx.execute(
                "INSERT OR IGNORE INTO message_tool_calls VALUES(?1,?2,?3,'result')",
                params![message.session_id, call, seq],
            )?;
        }
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn dispatch(
        &self,
        session_id: &str,
        run_id: &str,
        call: &ToolCall,
    ) -> Result<String> {
        if let Some(store) = self.scoped(session_id)? {
            return store.dispatch(session_id, run_id, call);
        }
        let invocation = id();
        self.write(|tx| {
            tx.execute(
                "INSERT INTO effects(id,session_id,run_id,call_id,state) VALUES(?1,?2,?3,?4,'dispatched')",
                params![invocation, session_id, run_id, call.id],
            )?;
            Ok(())
        })?;
        Ok(invocation)
    }
    pub(crate) fn dispatch_nested(
        &self,
        session: &str,
        run: &str,
        call: &ToolCall,
    ) -> Result<String> {
        if let Some(store) = self.scoped(session)? {
            return store.dispatch_nested(session, run, call);
        }
        let invocation = id();
        self.write(|tx| {
            let hash = Self::blob_put(tx, &serde_json::to_vec(&call.arguments)?)?;
            let detail = serde_json::json!({"id":call.id,"name":call.name,"_arguments_blob":hash});
            tx.execute("INSERT INTO effects(id,session_id,run_id,call_id,state,detail) VALUES(?1,?2,?3,?4,'dispatched',?5)", params![invocation,session,run,call.id,serde_json::to_string(&detail)?])?;
            Ok(())
        })?;
        Ok(invocation)
    }
    pub(crate) fn effect_completed(&self, invocation: &str) -> Result<()> {
        if self.0.layout.is_some() {
            return self
                .find_record("effects", invocation)?
                .effect_completed(invocation);
        }
        self.write(|tx| {
            tx.execute(
                "UPDATE effects SET state='completed' WHERE id=?1",
                [invocation],
            )?;
            Ok(())
        })
    }
    pub(crate) fn effect_unknown(&self, invocation: &str) -> Result<()> {
        if self.0.layout.is_some() {
            return self
                .find_record("effects", invocation)?
                .effect_unknown(invocation);
        }
        self.write(|tx| {
            tx.execute(
                "UPDATE effects SET state='unknown' WHERE id=?1 AND state='dispatched'",
                [invocation],
            )?;
            Ok(())
        })
    }
    pub fn outputs(&self, session: &str, limit: usize) -> Result<Vec<OutputRecord>> {
        if let Some(store) = self.scoped(session)? {
            return store.outputs(session, limit);
        }
        let conn = self.0.connection.lock().unwrap();
        let mut statement=conn.prepare("SELECT data FROM (SELECT seq,data FROM outputs WHERE session_id=?1 ORDER BY seq DESC LIMIT ?2) ORDER BY seq")?;
        statement
            .query_map(params![session, limit.min(512) as i64], |r| {
                r.get::<_, String>(0)
            })?
            .map(|r| Ok(serde_json::from_str(&r?)?))
            .collect()
    }
    /// Effect diagnostics are explicit, bounded and kept out of normal model history.
    pub fn pending_effects(&self, session: &str) -> Result<Vec<serde_json::Value>> {
        if let Some(store) = self.scoped(session)? {
            return store.pending_effects(session);
        }
        let conn = self.0.connection.lock().unwrap();
        let mut statement=conn.prepare("SELECT id,run_id,state,detail FROM effects WHERE session_id=?1 AND state!='completed' LIMIT 64")?;
        statement
            .query_map([session], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })?
            .map(|r| {
                let (id, run, state, detail) = r?;
                let call = detail
                    .and_then(|v| serde_json::from_str::<serde_json::Value>(&v).ok())
                    .map(|mut value| -> Result<_> {
                        if let Some(hash) =
                            value.get("_arguments_blob").and_then(|hash| hash.as_str())
                        {
                            value["arguments"] =
                                serde_json::from_slice(&Self::blob_get(&conn, hash)?)?;
                            value.as_object_mut().unwrap().remove("_arguments_blob");
                        }
                        Ok(value)
                    })
                    .transpose()?;
                Ok(serde_json::json!({"id":id,"run_id":run,"state":state,"call":call}))
            })
            .collect()
    }
    pub(crate) fn close_unresolved(&self, session_id: &str, run_id: &str) -> Result<()> {
        if let Some(store) = self.scoped(session_id)? {
            return store.close_unresolved(session_id, run_id);
        }
        let messages = self.messages(session_id)?;
        let mut pending = Vec::new();
        for msg in messages {
            if msg.role == Role::Assistant {
                pending.extend(msg.tool_calls);
            } else if let Some(call_id) = msg.tool_call_id {
                pending.retain(|c| c.id != call_id);
            }
        }
        if pending.is_empty() {
            return Ok(());
        }
        self.write(|tx|{for call in pending {let dispatched:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM effects WHERE run_id=?1 AND call_id=?2)",params![run_id,call.id],|r|r.get(0))?;let result=ToolResult::error(if dispatched {"Tool outcome unknown after interruption; effect may have occurred. Do not replay automatically."} else {"Tool was not dispatched because the run was interrupted."});let mut msg=Self::message(session_id,Role::Tool,result.content);msg.tool_call_id=Some(call.id);msg.is_error=true;Self::insert_message(tx,&msg)?;}tx.execute("UPDATE effects SET state='unknown' WHERE run_id=?1 AND state='dispatched'",[run_id])?;Ok(())})
    }
    pub fn fork(&self, session_id: &str, title: impl Into<String>) -> Result<Session> {
        self.fork_inner(session_id, None, title.into(), || Ok(()), |_| Ok(()))
    }
    /// Fork canonical history through `message_id`, including that message.
    /// The selected prefix must contain results for every tool call it retains.
    pub fn fork_at(
        &self,
        session_id: &str,
        message_id: &str,
        title: impl Into<String>,
    ) -> Result<Session> {
        self.fork_at_with(session_id, message_id, title, |_| Ok(()))
    }
    /// Fork and perform host work while source runtime admission remains excluded.
    /// The callback runs after the fork commits and may read Store or perform host
    /// work, but must not reenter runtime/admission operations (including fork,
    /// start, delete or compaction). Callback errors leave the fork committed.
    pub fn fork_at_with(
        &self,
        session_id: &str,
        message_id: &str,
        title: impl Into<String>,
        after: impl FnOnce(&Session) -> Result<()>,
    ) -> Result<Session> {
        self.fork_at_admitted(session_id, message_id, title, || Ok(()), after)
    }

    /// Like `fork_at_with`, but `admitted` runs once the source admission guard is
    /// held and before the fork is created. An error from it aborts the fork, so a
    /// check made there cannot be overtaken by a turn that starts in between.
    pub fn fork_at_admitted(
        &self,
        session_id: &str,
        message_id: &str,
        title: impl Into<String>,
        admitted: impl FnOnce() -> Result<()>,
        after: impl FnOnce(&Session) -> Result<()>,
    ) -> Result<Session> {
        self.fork_inner(session_id, Some(message_id), title.into(), admitted, after)
    }
    fn fork_inner(
        &self,
        session_id: &str,
        message_id: Option<&str>,
        title: String,
        admitted: impl FnOnce() -> Result<()>,
        after: impl FnOnce(&Session) -> Result<()>,
    ) -> Result<Session> {
        if self.0.layout.is_some() {
            return self.layout_fork(session_id, message_id, title, admitted, after);
        }
        let runtime = self.runtime();
        let _admission = runtime.fork_guard(session_id)?;
        admitted()?;
        self.session(session_id)?;
        let active_effect: bool = self.0.connection.lock().unwrap().query_row(
            "SELECT EXISTS(SELECT 1 FROM effects WHERE session_id=?1 AND state='dispatched')",
            [session_id],
            |r| r.get(0),
        )?;
        anyhow::ensure!(!active_effect, "Cannot fork with active background effects");
        let source = Self::fork_history(self.messages(session_id)?, message_id)?;
        let strategy = self.session(session_id)?.context_strategy;
        let mut session = self.create_session_with_strategy(title, strategy)?;
        session.parent_session_id = Some(session_id.into());
        session.parent_cutoff = Some(source.len() as i64);
        let execution_mode = self.initial_execution_mode(Some(session_id))?;
        self.write(|tx| {
            Self::insert_execution_mode(tx, &session.id, &execution_mode)?;
            tx.execute(
                "UPDATE sessions SET data=?2 WHERE id=?1",
                params![session.id, serde_json::to_string(&session)?],
            )?;
            for mut msg in source {
                let original_message_id = msg.id.clone();
                msg.id = id();
                msg.session_id = session.id.clone();
                Self::insert_message(tx, &msg)?;
                Self::copy_reminder_metadata_tx(
                    tx,
                    session_id,
                    &original_message_id,
                    &session.id,
                    &msg.id,
                )?;
            }
            Ok(())
        })?;
        after(&session)?;
        Ok(session)
    }
    fn fork_history(mut history: Vec<Message>, message_id: Option<&str>) -> Result<Vec<Message>> {
        if let Some(message_id) = message_id {
            let cutoff = history
                .iter()
                .position(|message| message.id == message_id)
                .context("Fork cutoff message is not in the source session")?;
            history.truncate(cutoff + 1);
        }
        anyhow::ensure!(
            !history
                .iter()
                .flat_map(|message| &message.tool_calls)
                .any(|call| !history
                    .iter()
                    .any(|message| message.tool_call_id.as_deref() == Some(&call.id))),
            "Cannot fork with unresolved tool calls"
        );
        Ok(history)
    }
    pub fn storage_stats(&self) -> Result<StorageStats> {
        if self.0.layout.is_some() {
            return self.layout_stats();
        }
        let conn = self.0.connection.lock().unwrap();
        let count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        let bytes = |path: &Path| std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        Ok(StorageStats {
            chat_bytes: self.0.path.as_ref().map(|p| bytes(p)).unwrap_or(0),
            serving_bytes: 0,
            catalog_bytes: 0,
            chat_transaction_count: self.0.transactions.load(Ordering::Relaxed),
            serving_transaction_count: 0,
            catalog_transaction_count: 0,
            page_bytes: (count * size) as u64,
            database_bytes: self.0.path.as_ref().map(|p| bytes(p)).unwrap_or(0),
            wal_bytes: self
                .0
                .path
                .as_ref()
                .map(|p| bytes(Path::new(&format!("{}-wal", p.display()))))
                .unwrap_or(0),
            transaction_count: self.0.transactions.load(Ordering::Relaxed),
            message_count: conn
                .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0))?
                as u64,
            pending_input_count: conn.query_row(
                "SELECT COUNT(*) FROM inputs WHERE state='pending'",
                [],
                |r| r.get::<_, i64>(0),
            )? as u64,
        })
    }
}

// Runtime batch helpers. They deliberately take the caller's transaction:
// sequence assignment and state changes must commit together under Log's lock.
impl Store {
    pub(crate) fn start_run_tx(tx: &Connection, run: &Run) -> Result<()> {
        tx.execute(
            "INSERT INTO runs(id,session_id,status,data) VALUES(?1,?2,'running',?3)",
            params![run.id, run.session_id, serde_json::to_string(run)?],
        )?;
        Ok(())
    }
    pub(crate) fn admit_tx(
        tx: &Connection,
        run: &Run,
        text: Option<&str>,
        origin: MessageOrigin,
    ) -> Result<()> {
        if Self::has_quota_wait_tx(tx, &run.session_id)? {
            // The host explicitly starts a fresh run from retained history.
            // Clearing in admission prevents a crash from scheduling it twice.
            Self::ensure_quota_effects_resolved(tx, &run.session_id)?;
            Self::clear_quota_wait_tx(tx, &run.session_id, None)?;
        }
        Self::start_run_tx(tx, run)?;
        if let Some(text) = text {
            let input = QueuedInput {
                id: id(),
                session_id: run.session_id.clone(),
                kind: InputKind::Followup,
                text: text.into(),
                state: "pending".into(),
                origin: Some(origin),
            };
            Self::queue_tx(tx, &input)?;
        }
        Self::consume_tx(tx, &run.session_id, false, true)?;
        Ok(())
    }
    pub(crate) fn queue_tx(tx: &Connection, input: &QueuedInput) -> Result<()> {
        let (hash, format) = Self::input_content_ref(tx, &input.text)?;
        tx.execute("INSERT INTO inputs(id,session_id,kind,text,state,origin,content_blob,content_format) VALUES(?1,?2,?3,'','pending',?4,?5,?6)",
            params![input.id, input.session_id, if input.kind == InputKind::Steer {"steer"} else {"followup"},
                input.origin.map(|origin| serde_json::to_string(&origin)).transpose()?, hash, format])?;
        tx.execute(
            "UPDATE sessions SET data=json_set(data,'$.updated_at',?2) WHERE id=?1",
            params![input.session_id, now()],
        )?;
        Ok(())
    }
    pub(crate) fn finish_run_tx(
        tx: &Connection,
        run: &Run,
        result_message_id: Option<&str>,
    ) -> Result<()> {
        if run.status != "interrupted" {
            Self::clear_quota_wait_tx(tx, &run.session_id, Some(&run.id))?;
        }
        tx.execute(
            "UPDATE runs SET status=?2,data=?3,result_message_id=?4 WHERE id=?1",
            params![
                run.id,
                run.status,
                Self::run_metadata(tx, run)?,
                result_message_id
            ],
        )?;
        Ok(())
    }
    pub(crate) fn dispatch_tx(
        tx: &Connection,
        session_id: &str,
        run_id: &str,
        call: &ToolCall,
    ) -> Result<String> {
        let invocation = id();
        tx.execute("INSERT INTO effects(id,session_id,run_id,call_id,state) VALUES(?1,?2,?3,?4,'dispatched')",
            params![invocation,session_id,run_id,call.id])?;
        Ok(invocation)
    }
    pub(crate) fn tool_completed_tx(
        tx: &Connection,
        message: &Message,
        invocation: Option<&str>,
        pending: bool,
    ) -> Result<()> {
        Self::insert_message(tx, message)?;
        if let Some(invocation) = invocation {
            tx.execute(
                "UPDATE effects SET state=CASE WHEN ?2 THEN state ELSE 'completed' END WHERE id=?1",
                params![invocation, pending],
            )?;
        }
        Ok(())
    }
    pub(crate) fn append_output_tx(tx: &Connection, record: &OutputRecord) -> Result<()> {
        let data = serde_json::to_string(record)?;
        anyhow::ensure!(data.len() <= 64 * 1024, "Output exceeds size limit");
        let (count,bytes): (i64,i64) = tx.query_row("SELECT COUNT(*),COALESCE(SUM(length(CAST(data AS BLOB))),0) FROM outputs WHERE execution_id=?1", [&record.execution_id], |row| Ok((row.get(0)?,row.get(1)?)))?;
        anyhow::ensure!(
            count < 128 && bytes + data.len() as i64 <= 256 * 1024,
            "Execution output exceeds size limit"
        );
        tx.execute(
            "INSERT INTO outputs(id,session_id,execution_id,data) VALUES(?1,?2,?3,?4)",
            params![record.id, record.session_id, record.execution_id, data],
        )?;
        Ok(())
    }
}

/// Attachment ID prefix that earlier builds gave checkpoint rows. Current builds
/// keep checkpoints in `checkpoint_records`; the one-time sweep deletes the old rows.
pub const CHECKPOINT_PREFIX: &str = "file-checkpoint-v1/";

/// A legacy checkpoint row that the sweep could not migrate cleanly, with the reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyCheckpointNote {
    pub session_id: String,
    pub id: String,
    /// True when the row stays in `attachments`; false when it was migrated as unrestorable.
    pub left: bool,
    pub reason: String,
}

/// One checkpoint record for `Store::write_checkpoint`. `boundary_seq` is the
/// canonical sequence of the message that owns the change, and it decides pruning.
/// `patch_index` numbers the patch within its session and must not go below the
/// session's high-water mark. `metadata` is opaque to the core.
#[derive(Clone, Copy, Debug)]
pub struct CheckpointRecord<'a> {
    pub id: &'a str,
    pub patch_index: u64,
    pub boundary_seq: i64,
    pub metadata: &'a str,
}

/// Checkpoint tables, created by record step 10 and by a fresh record. `checkpoint_blobs` holds
/// zstd-compressed preimages keyed by their plain SHA-256; `checkpoint_records`
/// holds one row per checkpoint and names its preimage. A NULL `preimage_hash` marks
/// an unrestorable record, which has no preimage. The index serves the reference
/// check made when records are pruned.
pub(crate) const CHECKPOINT_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS checkpoint_blobs(hash TEXT PRIMARY KEY,raw_bytes INTEGER NOT NULL,data BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS checkpoint_records(session_id TEXT NOT NULL REFERENCES sessions(id),id TEXT NOT NULL,boundary_seq INTEGER NOT NULL,metadata TEXT NOT NULL,preimage_hash TEXT REFERENCES checkpoint_blobs(hash),PRIMARY KEY(session_id,id));
CREATE INDEX IF NOT EXISTS checkpoint_records_preimage ON checkpoint_records(preimage_hash);
CREATE INDEX IF NOT EXISTS checkpoint_records_boundary ON checkpoint_records(session_id,boundary_seq);
";

/// Record ID of the unrestorable record the sweep writes for a session that has a
/// legacy row whose metadata cannot place it in a turn.
pub(crate) const UNREADABLE_RECORD_ID: &str = "unrestorable/legacy";

/// Record step 10 makes `preimage_hash` nullable on a table an earlier build
/// created `NOT NULL`; the rebuild copies every row. The column is checked once,
/// because a store the previous per-open upgrade already converted only gains the
/// version. A store at step 10 or later already has its tables, so it is not
/// inspected here.
pub(crate) fn migrate_checkpoint_schema(connection: &mut Connection, version: i64) -> Result<()> {
    if version >= 10 {
        return Ok(());
    }
    let migration = connection.transaction()?;
    migration.execute_batch(CHECKPOINT_SCHEMA)?;
    let not_null: bool = migration.query_row(
        "SELECT COALESCE((SELECT \"notnull\"=1 FROM pragma_table_info('checkpoint_records') WHERE name='preimage_hash'),0)",
        [],
        |row| row.get(0),
    )?;
    if not_null {
        migration.execute_batch(
            "CREATE TABLE checkpoint_records_relaxed(session_id TEXT NOT NULL REFERENCES sessions(id),id TEXT NOT NULL,boundary_seq INTEGER NOT NULL,metadata TEXT NOT NULL,preimage_hash TEXT REFERENCES checkpoint_blobs(hash),PRIMARY KEY(session_id,id));
            INSERT INTO checkpoint_records_relaxed SELECT session_id,id,boundary_seq,metadata,preimage_hash FROM checkpoint_records;
            DROP TABLE checkpoint_records;
            ALTER TABLE checkpoint_records_relaxed RENAME TO checkpoint_records;
            CREATE INDEX checkpoint_records_preimage ON checkpoint_records(preimage_hash);
            CREATE INDEX checkpoint_records_boundary ON checkpoint_records(session_id,boundary_seq);",
        )?;
    }
    migration.execute_batch("PRAGMA user_version=10;")?;
    migration.commit()?;
    Ok(())
}
