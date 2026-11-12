use crate::{ContextEvent, ContextEventKind, ContextSurfaceRebind};
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
#[path = "store_context_view.rs"]
mod context_view;
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
#[path = "store_schema.rs"]
mod schema;
pub use schema::StoreSchemaError;
#[path = "store_search.rs"]
mod search;
#[path = "store_serving.rs"]
mod serving;
#[path = "store_usage.rs"]
pub(crate) mod usage;
pub use search::{SearchHit, SearchIndexProgress};
#[path = "store_checkpoints.rs"]
mod checkpoints;
#[path = "store_import.rs"]
mod import;
#[path = "store_inputs.rs"]
mod inputs;
#[path = "store_messages.rs"]
mod messages;
#[path = "store_ownership.rs"]
mod ownership;
#[path = "store_recovery.rs"]
mod recovery;
#[path = "store_reminders.rs"]
mod reminders;
#[path = "store_session_ops.rs"]
mod session_ops;
#[cfg(test)]
#[path = "store_test_support.rs"]
mod test_support;
#[path = "store_workspace.rs"]
mod workspace;
pub(crate) use checkpoints::CHECKPOINT_SCHEMA;
pub use checkpoints::{CHECKPOINT_PREFIX, CheckpointRecord, LegacyCheckpointNote};
#[path = "store_blobs.rs"]
mod blobs;
#[path = "store_context_records.rs"]
mod context_records;
#[path = "store_context_types.rs"]
mod context_types;
#[path = "store_runs.rs"]
mod runs;
#[path = "store_tx.rs"]
mod tx;
#[path = "store_view_codec.rs"]
mod view_codec;
#[path = "store_view_sources.rs"]
mod view_sources;
pub use context_types::{
    ContextChapter, ContextDecision, ContextEpisode, DecisionKind, EpisodeBatch, RelatedEvidence,
    SubjectChapterStatus,
};
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

impl Store {
    /// One process owns a database. Repeated opens in this process share the connection.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if path.is_dir() {
            return Self::open_data_dir(path);
        }
        Self::open_data_dir(Self::legacy_data_dir(path))
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
        let fresh = application == 0;
        if !fresh {
            // Checked before any pragma touches the file, so a refused open changes nothing.
            let version: i64 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            StoreSchemaError::ensure("chat", version, schema::CHAT_VERSION)?;
        } else {
            connection.execute_batch("PRAGMA auto_vacuum=INCREMENTAL;")?;
        }
        connection.busy_timeout(std::time::Duration::from_secs(2))?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA wal_autocheckpoint=256; PRAGMA journal_size_limit=1048576;")?;
        if fresh {
            schema::create_chat(&mut connection)?;
        }
        // Additive tables without a version bump (docs/approvals.md).
        connection.execute_batch(crate::approvals::SCHEMA)?;
        connection.execute_batch(crate::event_log::SCHEMA)?;
        connection.execute_batch(crate::host_state::SCHEMA)?;
        Self::initialize_reminders(&connection)?;
        cold::initialize(&connection)?;
        let dictionary_pins = cold::pin_dictionaries(&connection)?;
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
