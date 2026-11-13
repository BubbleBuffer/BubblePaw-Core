//! Physical storage placement. Chat commits precede rebuildable projections.
use super::*;
use rusqlite::OpenFlags;
#[path = "store_layout_archive.rs"]
mod archive;
#[path = "store_layout_catalog.rs"]
mod catalog;
#[path = "store_layout_catalog_swap.rs"]
mod catalog_swap;
#[path = "store_layout_chat_open.rs"]
mod chat_open;
#[path = "store_layout_deletion.rs"]
mod deletion;
#[path = "store_exports.rs"]
mod exports;
#[path = "store_layout_housekeeping.rs"]
mod housekeeping;
#[path = "store_layout_open_dir.rs"]
mod open_dir;
#[path = "store_layout_release.rs"]
mod release;
#[path = "store_layout_routed.rs"]
mod routed;

type InputEventRow = (String, Option<String>, Option<String>, Option<String>);
#[cfg(test)]
thread_local! {
    static REFERENCE_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoreOptions {
    pub finished_event_retention_hours: u32,
    pub telemetry_retention_days: u32,
    pub archive_after_days: u32,
    pub cold_tail_messages: u32,
    pub cold_min_messages: u32,
    pub cold_min_bytes: u64,
    pub cold_row_min_bytes: u32,
}
/// Immutable storage policy for a root chat and its child sessions.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RootStoragePolicy {
    pub store: StoreOptions,
    /// None retains archives indefinitely. The host performs final deletion.
    pub archived_retention_days: Option<u32>,
}
impl RootStoragePolicy {
    pub(super) fn validate(&self) -> Result<()> {
        let instant = chrono::Utc::now();
        anyhow::ensure!(
            instant
                .checked_sub_signed(chrono::Duration::hours(i64::from(
                    self.store.finished_event_retention_hours
                )))
                .is_some(),
            "Finished event retention is outside the supported date range"
        );
        for days in [
            Some(self.store.telemetry_retention_days),
            Some(self.store.archive_after_days),
            self.archived_retention_days,
        ]
        .into_iter()
        .flatten()
        {
            anyhow::ensure!(
                instant
                    .checked_sub_signed(chrono::Duration::days(i64::from(days)))
                    .is_some(),
                "Storage retention is outside the supported date range"
            );
        }
        Ok(())
    }
}
impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            finished_event_retention_hours: 24,
            telemetry_retention_days: 30,
            archive_after_days: 30,
            cold_tail_messages: 128,
            cold_min_messages: 1024,
            cold_min_bytes: 8 * 1024 * 1024,
            cold_row_min_bytes: 256,
        }
    }
}
pub(super) struct Layout {
    pub data_dir: PathBuf,
    pub(super) catalog: Mutex<Connection>,
    chats: Mutex<HashMap<String, Store>>,
    /// Session id to root id, resolved from the catalog once per open chat.
    /// Placement never changes without a rebuild, archive restore or deletion
    /// dropping the affected entries, so a hit skips the catalog query.
    session_roots: Mutex<HashMap<String, String>>,
    options: StoreOptions,
    pub(super) catalog_transactions: AtomicU64,
    /// Catalog placement queries made by `scoped`, for tests that assert caching.
    pub(super) catalog_lookups: AtomicU64,
    pub(super) pending_catalog: Mutex<HashMap<String, (String, Session, String)>>,
    pub(super) projection_gap: std::sync::atomic::AtomicBool,
    pub(super) event_reserved_end: AtomicU64,
    pub(super) recovery: super::recovery::RecoveryEvents,
    pub(super) serving_transactions: AtomicU64,
    /// Roots whose chat files a delete is removing; they must not reopen.
    deleting: Mutex<std::collections::HashSet<String>>,
}

impl Layout {
    pub(super) fn options(&self) -> StoreOptions {
        self.options
    }
    /// The open chat for a resolved session, without touching the catalog.
    fn cached_chat(&self, session: &str) -> Option<Store> {
        let root = self.session_roots.lock().unwrap().get(session)?.clone();
        self.chats.lock().unwrap().get(&root).cloned()
    }
    fn remember_session(&self, session: &str, root: &str) {
        self.session_roots
            .lock()
            .unwrap()
            .insert(session.to_owned(), root.to_owned());
    }
    fn forget_sessions(&self, sessions: &[String]) {
        let mut roots = self.session_roots.lock().unwrap();
        for session in sessions {
            roots.remove(session);
        }
    }
    /// Placement for a root changed: later lookups must resolve through the catalog.
    fn forget_root(&self, root: &str) {
        self.session_roots
            .lock()
            .unwrap()
            .retain(|_, value| value != root);
    }
}
// `projection` creates a fresh file at its final version in one transaction and
// opens an existing one only at exactly that version.
const CATALOG_SCHEMA: &str = "PRAGMA application_id=1112556081;
CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE sessions(id TEXT PRIMARY KEY,parent_id TEXT,root_id TEXT NOT NULL,title TEXT NOT NULL,last_activity TEXT NOT NULL,created_at TEXT NOT NULL,file TEXT NOT NULL,storage_state TEXT NOT NULL DEFAULT 'active',data TEXT NOT NULL);
CREATE INDEX sessions_root ON sessions(root_id);";
const SERVING_SCHEMA: &str = "PRAGMA application_id=1112556082;
CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE sessions(id TEXT PRIMARY KEY,data TEXT NOT NULL);
CREATE TABLE context_views(session_id TEXT PRIMARY KEY,data TEXT NOT NULL);
CREATE TABLE runs(id TEXT PRIMARY KEY,session_id TEXT NOT NULL,status TEXT NOT NULL,data TEXT NOT NULL,result_message_id TEXT);
CREATE TABLE inputs(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE NOT NULL,session_id TEXT NOT NULL,kind TEXT NOT NULL,text TEXT NOT NULL,state TEXT NOT NULL,origin TEXT,message_id TEXT,content_blob TEXT,content_format TEXT,resolved_at TEXT NOT NULL,source_seq INTEGER NOT NULL);
CREATE TABLE approvals(id TEXT PRIMARY KEY,session_id TEXT NOT NULL,run_id TEXT NOT NULL,call_id TEXT NOT NULL,tool TEXT NOT NULL,arguments TEXT NOT NULL,state TEXT NOT NULL,reason TEXT,created_at TEXT NOT NULL,decided_at TEXT,arguments_ref TEXT);
CREATE TABLE telemetry(id INTEGER PRIMARY KEY,session_id TEXT,created_at TEXT NOT NULL,kind TEXT NOT NULL,data TEXT NOT NULL);
CREATE INDEX telemetry_created ON telemetry(created_at);
CREATE INDEX telemetry_record ON telemetry(session_id,json_extract(data,'$.record_id'));
CREATE TABLE events(seq INTEGER PRIMARY KEY,session_id TEXT NOT NULL,run_id TEXT NOT NULL,created_at TEXT NOT NULL,kind TEXT NOT NULL,data TEXT NOT NULL);
CREATE INDEX events_session ON events(session_id,seq);";

fn projection(
    path: &Path,
    application: i64,
    schema: &str,
    store: &'static str,
    expected: i64,
) -> Result<Connection> {
    let mut conn = Connection::open(path)?;
    let current: i64 = conn.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let tables: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        current == application || (current == 0 && tables == 0),
        "Foreign storage projection: {}",
        path.display()
    );
    let fresh = current == 0;
    if !fresh {
        // Checked before any pragma touches the file, so a refused open changes nothing.
        StoreSchemaError::ensure(store, version, expected)?;
    }
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA wal_autocheckpoint=256; PRAGMA journal_size_limit=1048576;")?;
    if fresh {
        let tx = conn.transaction()?;
        tx.execute_batch(schema)?;
        if application == 0x42504231 {
            tx.execute_batch(super::quota::QUOTA_SCHEMA)?;
        }
        if application == 0x42504232 {
            tx.execute_batch(super::usage::prompt_log::TABLES)?;
        }
        tx.execute_batch(&format!("PRAGMA user_version={expected};"))?;
        tx.commit()?;
    }
    if application == 0x42504232 {
        super::usage::prompt_log::setup(&conn)?;
        Store::search_schema(&conn)?;
        conn.execute(
            "ATTACH DATABASE ?1 AS catalog_projection",
            [path.with_file_name("catalog.db").to_string_lossy().as_ref()],
        )?;
        conn.execute_batch("PRAGMA catalog_projection.synchronous=NORMAL;")?;
    }
    Ok(conn)
}

/// All record readers/writers enter through this seam. Archived read-only
/// snapshots unpack outside the layout and live access restores through its gateway.
pub(super) struct RecordConnection {
    connection: Option<Connection>,
    dictionary_pins: cold::DictionaryPins,
    temporary: Option<PathBuf>,
}
impl std::ops::Deref for RecordConnection {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.connection.as_ref().unwrap()
    }
}
impl RecordConnection {
    fn into_connection(mut self) -> Connection {
        self.connection.take().unwrap()
    }
}
impl Drop for RecordConnection {
    fn drop(&mut self) {
        drop(self.connection.take());
        if let Some(path) = self.temporary.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}
pub(super) fn open_record(
    path: &Path,
    read_only: bool,
    immutable_allowed: bool,
) -> Result<RecordConnection> {
    let archive = cold::archive_path(path);
    let temporary = if !path.is_file() && read_only && archive.is_file() {
        let target = std::env::temp_dir().join(format!("bubblepaw-record-{}.db", id()));
        if let Err(error) = cold::unpack(&archive, &target) {
            let _ = std::fs::remove_file(&target);
            return Err(error);
        }
        Some(target)
    } else {
        None
    };
    let actual = temporary.as_deref().unwrap_or(path);
    if !read_only {
        return Ok(RecordConnection {
            connection: Some(Connection::open(actual)?),
            dictionary_pins: Arc::default(),
            temporary,
        });
    }
    let conn = match open_read_only(actual, immutable_allowed || temporary.is_some()) {
        Ok(conn) => conn,
        Err(error) => {
            if let Some(path) = temporary {
                let _ = std::fs::remove_file(path);
            }
            return Err(error);
        }
    };
    let mut record = RecordConnection {
        connection: Some(conn),
        dictionary_pins: Arc::default(),
        temporary,
    };
    let conn = &*record;
    let application: i64 = conn.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    anyhow::ensure!(application == 0x42504230, "Unsupported chat record schema");
    StoreSchemaError::ensure("chat", version, schema::CHAT_VERSION)?;
    record.dictionary_pins = cold::pin_dictionaries(&record)?;
    Ok(record)
}
fn open_read_only(path: &Path, immutable_allowed: bool) -> Result<Connection> {
    let wal = PathBuf::from(format!("{}-wal", path.display()));
    if !immutable_allowed || std::fs::metadata(wal).is_ok_and(|metadata| metadata.len() > 0) {
        // A live WAL must participate in SQLite's read snapshot.
        return Ok(Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?);
    }
    // On an idle checkpointed chat, immutable avoids creating empty WAL/SHM
    // sidecars in the user's directory. URI metacharacters must be escaped.
    let filename = path
        .to_string_lossy()
        .replace('%', "%25")
        .replace('?', "%3F")
        .replace('#', "%23");
    Ok(Connection::open_with_flags(
        format!("file:{filename}?immutable=1"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?)
}

impl Store {
    /// Read a directory store without opening a host, migrating schemas,
    /// restoring archives, rebuilding projections or applying retention.
    pub fn read_request_usage_data_dir(data_dir: impl AsRef<Path>) -> Result<Vec<RequestUsage>> {
        let data_dir = data_dir.as_ref();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(data_dir.join("store.lock"))
            .ok()
            .and_then(|file| FileLock::acquire_shared(file).ok());
        let immutable_allowed = lock.is_some();
        let mut paths = std::collections::BTreeSet::new();
        for entry in std::fs::read_dir(data_dir.join("chats"))? {
            let mut path = entry?.path();
            if path.to_string_lossy().ends_with(".db.zst") {
                path = path.with_extension("");
            } else if path.extension().is_none_or(|extension| extension != "db") {
                continue;
            }
            paths.insert(path);
        }
        let mut records = Vec::new();
        for path in paths {
            let connection = open_record(&path, true, immutable_allowed)?;
            records.extend(Self::request_usage_from_connection(&connection)?);
        }
        records.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.request_id.cmp(&right.request_id))
        });
        Ok(records)
    }
}

/// One session-placement gateway for cached host access and standalone export.
/// Only bootstrap/rebuild callers may bypass the catalog and scan/create files.
fn open_session_record<T>(
    catalog: &Mutex<Connection>,
    data_dir: &Path,
    session: &str,
    open: impl FnOnce(&str, &Path) -> Result<T>,
) -> Result<Option<(T, String)>> {
    let placement: Option<(String, String, String)> = catalog
        .lock()
        .unwrap()
        .query_row(
            "SELECT root_id,file,storage_state FROM sessions WHERE id=?1",
            [session],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((root, file, state)) = placement else {
        return Ok(None);
    };
    anyhow::ensure!(
        ["active", "archiving", "archived"].contains(&state.as_str()),
        "Unsupported chat storage state: {state}"
    );
    let relative = Path::new(&file);
    anyhow::ensure!(
        relative
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_))),
        "Invalid catalog chat path"
    );
    let path = data_dir.join(relative);
    let archive = cold::archive_path(&path);
    if !path.is_file() && !archive.is_file() {
        return Ok(None);
    }
    anyhow::ensure!(
        (if path.is_file() { &path } else { &archive })
            .canonicalize()?
            .starts_with(data_dir.canonicalize()?),
        "Chat path escapes data directory"
    );
    Ok(Some((open(&root, &path)?, root)))
}

// FULL record commits are already durable in their WAL. Routine turn completion
// keeps at most the configured 1 MiB journal budget before reclaiming it; forcing
// the public idle checkpoint still synchronizes and truncates every open file.
fn wal_needs_checkpoint(path: Option<&Path>) -> bool {
    path.is_some_and(|path| {
        std::fs::metadata(format!("{}-wal", path.display()))
            .is_ok_and(|metadata| metadata.len() >= 1_048_576)
    })
}

#[cfg(test)]
#[path = "store_layout_ownership_tests.rs"]
mod ownership_tests;

#[cfg(test)]
#[path = "store_layout_checkpoint_tests.rs"]
mod checkpoint_prune_tests;
