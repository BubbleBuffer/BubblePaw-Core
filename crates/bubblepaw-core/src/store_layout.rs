//! Physical storage placement. Chat commits precede rebuildable projections.
use super::*;
use rusqlite::OpenFlags;
#[path = "store_exports.rs"]
mod exports;
#[path = "store_layout_release.rs"]
mod release;

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
    chat_open_hooks: Mutex<Vec<(&'static str, ChatOpenHook)>>,
}

/// Runs once for each chat store, the first time the process opens it. A failed
/// hook fails that open. Hooks may use chat-store methods, but never open chats.
pub type ChatOpenHook = Arc<dyn Fn(&Store) -> Result<()> + Send + Sync>;

/// A chat that was already open when a hook was registered, and for which the hook
/// failed. The other open chats still ran it.
#[derive(Debug)]
pub struct ChatOpenFailure {
    pub chat: PathBuf,
    pub error: anyhow::Error,
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
// The catalog version is set by `projection`, inside the same transaction as its migration.
const CATALOG_SCHEMA: &str = "PRAGMA application_id=1112556081;
CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY,parent_id TEXT,root_id TEXT NOT NULL,title TEXT NOT NULL,last_activity TEXT NOT NULL,created_at TEXT NOT NULL,file TEXT NOT NULL,storage_state TEXT NOT NULL DEFAULT 'active',data TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS sessions_root ON sessions(root_id);";
const SERVING_SCHEMA: &str = "PRAGMA application_id=1112556082;
CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY,data TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS context_views(session_id TEXT PRIMARY KEY,data TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS runs(id TEXT PRIMARY KEY,session_id TEXT NOT NULL,status TEXT NOT NULL,data TEXT NOT NULL,result_message_id TEXT);
CREATE TABLE IF NOT EXISTS inputs(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE NOT NULL,session_id TEXT NOT NULL,kind TEXT NOT NULL,text TEXT NOT NULL,state TEXT NOT NULL,origin TEXT,message_id TEXT,content_blob TEXT,content_format TEXT,resolved_at TEXT NOT NULL,source_seq INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS approvals(id TEXT PRIMARY KEY,session_id TEXT NOT NULL,run_id TEXT NOT NULL,call_id TEXT NOT NULL,tool TEXT NOT NULL,arguments TEXT NOT NULL,state TEXT NOT NULL,reason TEXT,created_at TEXT NOT NULL,decided_at TEXT,arguments_ref TEXT);
CREATE TABLE IF NOT EXISTS telemetry(id INTEGER PRIMARY KEY,session_id TEXT,created_at TEXT NOT NULL,kind TEXT NOT NULL,data TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS telemetry_created ON telemetry(created_at);
CREATE INDEX IF NOT EXISTS telemetry_record ON telemetry(session_id,json_extract(data,'$.record_id'));
CREATE TABLE IF NOT EXISTS events(seq INTEGER PRIMARY KEY,session_id TEXT NOT NULL,run_id TEXT NOT NULL,created_at TEXT NOT NULL,kind TEXT NOT NULL,data TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS events_session ON events(session_id,seq);";

fn projection(path: &Path, application: i64, schema: &str) -> Result<Connection> {
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
    // Each projection keeps its own numbered chain: catalog v2 adds
    // `quota_history.balance`, serving v2 adds the request log's later columns.
    let supported = 2;
    anyhow::ensure!(
        version <= supported && (tables == 0 || version >= 1),
        "Unsupported projection schema version {version}"
    );
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA wal_autocheckpoint=256; PRAGMA journal_size_limit=1048576;")?;
    if application == 0x42504231 && version < 2 {
        // Catalog step 2 adds the balance column to a v1 catalog, whether or not the
        // previous per-open upgrade already added it.
        let tx = conn.transaction()?;
        tx.execute_batch(schema)?;
        tx.execute_batch(super::quota::QUOTA_SCHEMA)?;
        super::quota::migrate_catalog(&tx)?;
        tx.execute_batch("PRAGMA user_version=2;")?;
        tx.commit()?;
    } else {
        conn.execute_batch(schema)?;
        if tables == 0 {
            conn.execute_batch("PRAGMA user_version=1;")?;
        }
        if application == 0x42504231 {
            conn.execute_batch(super::quota::QUOTA_SCHEMA)?;
        }
    }
    if application == 0x42504232 {
        super::usage::prompt_log::setup(&conn)?;
        if version < 2 {
            // Serving step 2 adds the request log columns introduced after the first
            // release; setup has created the log by now.
            let tx = conn.transaction()?;
            super::usage::prompt_log::migrate(&tx)?;
            tx.execute_batch("PRAGMA user_version=2;")?;
            tx.commit()?;
        }
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
    anyhow::ensure!(
        application == 0x42504230 && (1..=12).contains(&version),
        "Unsupported chat record schema"
    );
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

struct InstallLock {
    file: Option<FileLock>,
    path: PathBuf,
    created: bool,
    installed: bool,
}
impl InstallLock {
    fn acquire(target: &Path) -> Result<Self> {
        std::fs::create_dir_all(target)?;
        let path = target.join("store.lock");
        let opened = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path);
        let (file, created) = match opened {
            Ok(file) => (file, true),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)?,
                false,
            ),
            Err(error) => return Err(error.into()),
        };
        let file = FileLock::acquire(
            file,
            "Migration destination is already owned by another host process",
        )?;
        Ok(Self {
            file: Some(file),
            path,
            created,
            installed: false,
        })
    }
}
impl Drop for InstallLock {
    fn drop(&mut self) {
        drop(self.file.take());
        if self.created && !self.installed {
            // Failed installation has no completed-layout marker. Remove only
            // the lock we created; never unlink a lock another host now owns.
            if let Ok(file) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.path)
                && fs2::FileExt::try_lock_exclusive(&file).is_ok()
                && !self.path.with_file_name("catalog.db").exists()
            {
                drop(file);
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

impl Store {
    /// Snapshot and split a legacy database without modifying its bytes or WAL.
    /// The completed directory is installed only after every chat commit succeeds.
    pub fn migrate_legacy(source: impl AsRef<Path>, data_dir: impl AsRef<Path>) -> Result<()> {
        let source = source.as_ref();
        let target = data_dir.as_ref();
        if !source.exists() {
            return Ok(());
        }
        let canonical_path = source.canonicalize()?;
        let canonical = canonical_path.to_string_lossy().into_owned();
        if target.join("store.lock").exists() && !target.join("catalog.db").exists() {
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(target.join("store.lock"))?;
            let _lock = FileLock::acquire(
                lock,
                "Migration destination is already owned by another host process",
            )?;
            return Ok(());
        }
        if target.join("catalog.db").exists() {
            let catalog = Connection::open_with_flags(
                target.join("catalog.db"),
                OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            let migrated: Option<String> = catalog
                .query_row(
                    "SELECT value FROM metadata WHERE key='legacy_source'",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(migrated) = migrated {
                anyhow::ensure!(
                    migrated == canonical,
                    "Data directory was migrated from another source"
                );
            }
            return Ok(());
        }
        // Use the exact legacy engine ownership path, including canonicalizing
        // aliases, and retain the lease until the destination receipt commits.
        let source_lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(canonical_path.with_extension("sqlite.lock"))?;
        let _source_lock = FileLock::acquire(
            source_lock,
            "Legacy database is already owned by another host process",
        )?;
        let parent = target
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        let staging = parent.join(format!(".bubblepaw-migrate-{}", id()));
        std::fs::create_dir(&staging)?;
        let result = (|| -> Result<()> {
            let original = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let application: i64 =
                original.query_row("PRAGMA application_id", [], |row| row.get(0))?;
            let tables: i64 = original.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'", [], |row| row.get(0))?;
            if application == 0 && tables == 0 {
                return Ok(());
            }
            anyhow::ensure!(
                application == 0x42504230,
                "Foreign SQLite database; refusing migration"
            );
            let snapshot = staging.join("legacy.snapshot.db");
            // VACUUM INTO includes committed WAL pages and only writes its destination.
            original.execute("VACUUM INTO ?1", [snapshot.to_string_lossy().as_ref()])?;
            drop(original);
            let legacy = Self::initialize(Connection::open(&snapshot)?, Some(snapshot.clone()))?;
            let directory = staging.join("data");
            let host = Self::open_data_dir(&directory)?;
            let sessions = legacy.list_sessions()?;
            // Legacy parent links represent forks, not physical placement.
            // Import each historical session into its own record file.
            let roots = sessions
                .iter()
                .map(|session| (session.id.clone(), vec![session.id.clone()]));
            legacy.read(|conn| {
                host.write(|tx| {
                    for table in ["events", "context_views"] {
                        Self::copy_rows(conn, tx, table, "", &[])?;
                    }
                    for key in ["events_last_seq", "events_pruned_through", "workspace"] {
                        let value: Option<String> = conn
                            .query_row("SELECT value FROM metadata WHERE key=?1", [key], |row| {
                                row.get(0)
                            })
                            .optional()?;
                        if let Some(value) = value {
                            tx.execute(
                                "INSERT OR REPLACE INTO metadata VALUES(?1,?2)",
                                params![key, value],
                            )?;
                        }
                    }
                    Ok(())
                })
            })?;
            for (root, ids) in roots {
                let chat = host.bootstrap_chat(&root)?;
                let source_conn = legacy.0.connection.lock().unwrap();
                chat.write_context(|tx| {
                    // Foreign keys remain enabled; copy dependency order and original
                    // seq values so summary and legacy view references stay exact.
                    for table in [
                        "metadata",
                        "sessions",
                        "blobs",
                        "message_dictionaries",
                        "messages",
                        "message_tool_calls",
                        "message_headers",
                        "runs",
                        "request_usage",
                        "inputs",
                        "reminder_metadata",
                        "effects",
                        "outputs",
                        "context_checkpoints",
                        "context_chapters",
                        "context_subject_chapters",
                        "context_episodes",
                        "context_decisions",
                        "context_events",
                        "context_checkpoint_versions",
                        "attachments",
                        "approvals",
                        "approval_rules",
                        "host_state",
                    ] {
                        let filter =
                            if ["metadata", "blobs", "message_dictionaries"].contains(&table) {
                                String::new()
                            } else {
                                let key = if table == "sessions" {
                                    "id"
                                } else {
                                    "session_id"
                                };
                                format!(
                                    " WHERE {key} IN ({})",
                                    ids.iter().map(|_| "?").collect::<Vec<_>>().join(",")
                                )
                            };
                        Self::copy_rows(
                            &source_conn,
                            tx,
                            table,
                            &filter,
                            if filter.is_empty() { &[] } else { &ids },
                        )?;
                    }
                    cold::pin_all_into(tx, &chat.0.dictionary_pins)?;
                    chat.collect_orphan_blobs(tx)?;
                    tx.execute(
                        "INSERT OR REPLACE INTO metadata VALUES('legacy_source',?1)",
                        [&canonical],
                    )?;
                    Ok(())
                })?;
                chat.archive_operations()?;
            }
            host.catalog_rebuild()?;
            let layout = host.0.layout.as_ref().unwrap();
            layout.catalog.lock().unwrap().execute(
                "INSERT OR REPLACE INTO metadata VALUES('legacy_source',?1)",
                [&canonical],
            )?;
            host.layout_checkpoint()?;
            drop(host);
            drop(legacy);
            Self::install_migration(&directory, target, &canonical)?;
            Ok(())
        })();
        // Staging belongs to this migration and contains no original user files.
        let _ = std::fs::remove_dir_all(&staging);
        result
    }
    fn install_migration(directory: &Path, target: &Path, canonical: &str) -> Result<()> {
        let mut owner = InstallLock::acquire(target)?;
        // Recheck after taking the actual destination lease: another startup or
        // migrator could have completed while this source snapshot was built.
        let catalog_path = target.join("catalog.db");
        if catalog_path.exists() {
            let catalog = open_read_only(&catalog_path, false)?;
            let migrated: Option<String> = catalog
                .query_row(
                    "SELECT value FROM metadata WHERE key='legacy_source'",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            anyhow::ensure!(
                migrated.as_deref() == Some(canonical),
                "Migration destination was initialized from another source or by another host"
            );
            owner.installed = true;
            return Ok(());
        }
        // The old archive/attachments can already live inside data_dir.
        // Install complete per-chat files first; the catalog receipt is last.
        std::fs::create_dir_all(target.join("chats"))?;
        for entry in std::fs::read_dir(directory.join("chats"))? {
            let path = entry?.path();
            let destination = target.join("chats").join(path.file_name().unwrap());
            if destination.exists() {
                let existing =
                    Connection::open_with_flags(&destination, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                let provenance: Option<String> = existing
                    .query_row(
                        "SELECT value FROM metadata WHERE key='legacy_source'",
                        [],
                        |row| row.get(0),
                    )
                    .optional()?;
                anyhow::ensure!(
                    provenance.as_deref() == Some(canonical),
                    "Migration would overwrite an unrelated chat"
                );
                // A prior interrupted installation already committed this
                // whole chat. Preserve any accepted input appended since.
                continue;
            }
            std::fs::rename(path, destination)?;
        }
        for name in ["serving.db", "catalog.db"] {
            let destination = target.join(name);
            if destination.exists() && name == "serving.db" {
                let existing =
                    Connection::open_with_flags(&destination, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                let application: i64 =
                    existing.query_row("PRAGMA application_id", [], |row| row.get(0))?;
                anyhow::ensure!(
                    application == 0x42504232,
                    "Foreign serving database blocks migration"
                );
                drop(existing);
                for suffix in ["", "-wal", "-shm"] {
                    let path = PathBuf::from(format!("{}{suffix}", destination.display()));
                    if path.exists() {
                        std::fs::remove_file(path)?;
                    }
                }
            }
            anyhow::ensure!(
                !destination.exists(),
                "Migration destination already exists: {}",
                destination.display()
            );
            std::fs::rename(directory.join(name), destination)?;
        }
        owner.installed = true;
        Ok(())
    }
    fn copy_rows(
        source: &Connection,
        destination: &Connection,
        table: &str,
        filter: &str,
        ids: &[String],
    ) -> Result<()> {
        let mut query = source.prepare(&format!("SELECT * FROM {table}{filter}"))?;
        let columns = query
            .column_names()
            .iter()
            .map(|column| format!("\"{column}\""))
            .collect::<Vec<_>>();
        let sql = format!(
            "INSERT OR REPLACE INTO {table}({}) VALUES({})",
            columns.join(","),
            columns.iter().map(|_| "?").collect::<Vec<_>>().join(",")
        );
        let mut rows = query.query(rusqlite::params_from_iter(ids))?;
        while let Some(row) = rows.next()? {
            let values = (0..columns.len())
                .map(|index| row.get::<_, rusqlite::types::Value>(index))
                .collect::<rusqlite::Result<Vec<_>>>()?;
            destination.execute(&sql, rusqlite::params_from_iter(values))?;
        }
        Ok(())
    }
    pub fn legacy_data_dir(path: impl AsRef<Path>) -> PathBuf {
        let path = path.as_ref();
        path.with_file_name(format!(
            "{}.data",
            path.file_stem()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
        ))
    }
    pub fn open_data_dir(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_options(path, StoreOptions::default())
    }
    pub fn open_with_options(path: impl AsRef<Path>, options: StoreOptions) -> Result<Self> {
        std::fs::create_dir_all(path.as_ref().join("chats"))?;
        let data_dir = path.as_ref().canonicalize()?;
        let mut owners = OWNERS.get_or_init(Mutex::default).lock().unwrap();
        if let Some(owner) = owners.get(&data_dir) {
            if let Some(inner) = owner.inner.upgrade() {
                return Ok(Self(inner));
            }
            // A dead Weak proves there are no users, but Inner may still be
            // flushing/closing on the thread that dropped the final Store.
            owner.released.wait();
        }
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(data_dir.join("store.lock"))?;
        let lock = OwnershipLease::new(FileLock::acquire(
            lock,
            "Store is already owned by another host process",
        )?);
        let released = lock.released.clone();
        let catalog = projection(&data_dir.join("catalog.db"), 0x42504231, CATALOG_SCHEMA)?;
        let serving = projection(&data_dir.join("serving.db"), 0x42504232, SERVING_SCHEMA)?;
        let (last, pruned) = crate::event_log::bounds(&serving)?;
        let store = Self(Arc::new(Inner {
            connection: Arc::new(Mutex::new(serving)),
            dictionary_pins: Arc::default(),
            path: Some(data_dir.join("serving.db")),
            layout: Some(Arc::new(Layout {
                data_dir: data_dir.clone(),
                catalog: Mutex::new(catalog),
                chats: Mutex::default(),
                session_roots: Mutex::default(),
                options,
                catalog_transactions: AtomicU64::new(0),
                catalog_lookups: AtomicU64::new(0),
                pending_catalog: Mutex::default(),
                projection_gap: std::sync::atomic::AtomicBool::new(false),
                event_reserved_end: AtomicU64::new(0),
                recovery: Default::default(),
                serving_transactions: AtomicU64::new(0),
                chat_open_hooks: Mutex::default(),
            })),
            host: None,
            transactions: Arc::new(AtomicU64::new(0)),
            write_nanoseconds: Arc::new(AtomicU64::new(0)),
            runtime: Arc::default(),
            pending_usage: Arc::default(),
            _lock: Some(lock),
        }));
        store.runtime().log.seed(last, pruned);
        // Scanning also repairs stale catalog entries left by interrupted projection writes.
        store.catalog_rebuild_mode(false)?;
        store.initialize_event_sequences()?;
        store.publish_startup_recovery_events()?;
        store.housekeeping()?;
        owners.insert(
            data_dir,
            Owner {
                inner: Arc::downgrade(&store.0),
                released,
            },
        );
        Ok(store)
    }
    pub fn data_dir(&self) -> Option<&Path> {
        self.0
            .layout
            .as_ref()
            .map(|layout| layout.data_dir.as_path())
    }
    pub(super) fn host(&self) -> Option<Self> {
        self.0.host.as_ref().and_then(Weak::upgrade).map(Self)
    }
    pub(crate) fn is_layout(&self) -> bool {
        self.0.layout.is_some()
    }
    pub(crate) fn scoped(&self, session: &str) -> Result<Option<Self>> {
        let Some(layout) = &self.0.layout else {
            return Ok(None);
        };
        let open = |root: &str, path: &Path| {
            self.open_chat_file(
                root,
                path.strip_prefix(&layout.data_dir)?
                    .to_str()
                    .context("Chat path is not UTF-8")?,
                false,
            )
        };
        if let Some(store) = layout.cached_chat(session) {
            return Ok(Some(store));
        }
        layout.catalog_lookups.fetch_add(1, Ordering::Relaxed);
        let store = match open_session_record(&layout.catalog, &layout.data_dir, session, open)? {
            Some((store, root)) => {
                layout.remember_session(session, &root);
                store
            }
            None => {
                self.catalog_rebuild()?;
                layout.catalog_lookups.fetch_add(1, Ordering::Relaxed);
                let (store, root) =
                    open_session_record(&layout.catalog, &layout.data_dir, session, open)?
                        .ok_or(rusqlite::Error::QueryReturnedNoRows)
                        .with_context(|| {
                            format!("Unknown session or missing chat file: {session}")
                        })?;
                layout.remember_session(session, &root);
                store
            }
        };
        Ok(Some(store))
    }
    /// Catalog placement queries performed by `scoped` in this process.
    #[doc(hidden)]
    pub fn catalog_lookups(&self) -> u64 {
        self.0
            .layout
            .as_ref()
            .map_or(0, |layout| layout.catalog_lookups.load(Ordering::Relaxed))
    }
    fn open_chat(&self, root: &str) -> Result<Self> {
        self.open_chat_mode(root, false)
    }
    pub(super) fn bootstrap_chat(&self, root: &str) -> Result<Self> {
        self.open_chat_mode(root, true)
    }
    fn open_chat_mode(&self, root: &str, create: bool) -> Result<Self> {
        self.open_chat_file(root, &format!("chats/{root}.db"), create)
    }
    fn open_chat_file(&self, root: &str, file: &str, create: bool) -> Result<Self> {
        let layout = self.0.layout.as_ref().context("Not a directory store")?;
        anyhow::ensure!(
            !root.is_empty()
                && root
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "Invalid root session ID"
        );
        let path = layout.data_dir.join(file);
        anyhow::ensure!(
            path.parent()
                .context("Chat directory missing")?
                .canonicalize()?
                .starts_with(&layout.data_dir),
            "Chat path escapes data directory"
        );
        if path.exists() {
            anyhow::ensure!(
                path.canonicalize()?.starts_with(&layout.data_dir),
                "Chat path escapes data directory"
            );
        }
        let mut chats = layout.chats.lock().unwrap();
        if let Some(store) = chats.get(root) {
            anyhow::ensure!(
                store.0.path.as_ref() == Some(&path),
                "Catalog placement changed while chat was open"
            );
            return Ok(store.clone());
        }
        if !path.is_file() && cold::archive_path(&path).is_file() {
            anyhow::ensure!(
                cold::archive_path(&path)
                    .canonicalize()?
                    .starts_with(&layout.data_dir),
                "Archived chat path escapes data directory"
            );
            self.restore_chat(root, &path)?;
        }
        anyhow::ensure!(
            create || path.is_file(),
            "Chat record file is missing: {}",
            path.display()
        );
        let mut connection = open_record(&path, false, false)?.into_connection();
        let split: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='metadata')",
            [],
            |row| row.get(0),
        )?;
        let split = split && connection.query_row("SELECT EXISTS(SELECT 1 FROM metadata WHERE key='storage_layout' AND value='chat-v1')", [], |row| row.get::<_, bool>(0))?;
        let temporary = if split {
            let application: i64 =
                connection.query_row("PRAGMA application_id", [], |row| row.get(0))?;
            let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            anyhow::ensure!(
                application == 0x42504230 && (7..=12).contains(&version),
                "Unsupported chat record schema"
            );
            connection.busy_timeout(std::time::Duration::from_secs(2))?;
            connection.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA wal_autocheckpoint=256; PRAGMA journal_size_limit=1048576;")?;
            Self::migrate_request_usage(&mut connection, version)?;
            Self::initialize_reminders(&connection)?; // existing chat-v1 records skip initialize
            crate::store::migrate_checkpoint_schema(&mut connection, version)?;
            cold::initialize(&connection)?;
            let dictionary_pins = cold::pin_dictionaries(&connection)?;
            Self::initialize_tool_call_index(&mut connection)?;
            Self::migrate_context_facts(&mut connection, version)?;
            Self(Arc::new(Inner {
                connection: Arc::new(Mutex::new(connection)),
                dictionary_pins,
                path: Some(path.clone()),
                layout: None,
                host: None,
                transactions: Arc::new(AtomicU64::new(0)),
                write_nanoseconds: Arc::new(AtomicU64::new(0)),
                runtime: Arc::default(),
                pending_usage: Arc::default(),
                _lock: None,
            }))
        } else {
            let store = Self::initialize(connection, Some(path.clone()))?;
            store.read(|conn| {
                conn.execute_batch(
                    "DROP TABLE IF EXISTS events; DROP TABLE IF EXISTS context_views;",
                )?;
                conn.execute(
                    "INSERT OR REPLACE INTO metadata VALUES('storage_layout','chat-v1')",
                    [],
                )?;
                Ok(())
            })?;
            store
        };
        temporary.read(|conn| {
            // No serving content is persisted in a chat. Temporary empty tables
            // retain compatibility for proven legacy deletion/recovery helpers.
            conn.execute_batch("CREATE TEMP TABLE events(seq INTEGER PRIMARY KEY,session_id TEXT,run_id TEXT,created_at TEXT,kind TEXT,data TEXT);
                CREATE TEMP TABLE context_views(session_id TEXT PRIMARY KEY,data TEXT);")?;
            Ok(())
        })?;
        let store = Self(Arc::new(Inner {
            connection: temporary.0.connection.clone(),
            dictionary_pins: temporary.0.dictionary_pins.clone(),
            path: Some(path),
            layout: None,
            host: Some(Arc::downgrade(&self.0)),
            transactions: self.0.transactions.clone(),
            write_nanoseconds: self.0.write_nanoseconds.clone(),
            runtime: self.runtime(),
            pending_usage: temporary.0.pending_usage.clone(),
            _lock: None,
        }));
        // Chat-open hooks, such as the legacy checkpoint sweep, run before the chat is
        // published. The chats lock stays held, so no other caller can receive this
        // chat until its migration has run. A failing hook fails the open and leaves
        // the chat unpublished, so the next open attempts the migration again.
        let hooks = layout.chat_open_hooks.lock().unwrap().clone();
        for (_, hook) in hooks {
            hook(&store)?;
        }
        chats.insert(root.into(), store.clone());
        drop(chats);
        store.finish_chat_open_recovery(split)?;
        Ok(store)
    }

    /// Register a hook under `key` that runs when a chat store is first opened in
    /// this process. Chats that the store constructor already opened (active roots
    /// are scanned at open) are cached before any hook exists, so the hook runs
    /// on those now, once each. A failing hook on one of them does not stop the
    /// others; each failure is returned, so the caller can report it. A key already
    /// registered is ignored, so callers may register on every construction. Only a
    /// directory store takes hooks.
    pub fn on_chat_open(
        &self,
        key: &'static str,
        hook: ChatOpenHook,
    ) -> Result<Vec<ChatOpenFailure>> {
        let layout = self.0.layout.as_ref().context("Not a directory store")?;
        {
            let mut hooks = layout.chat_open_hooks.lock().unwrap();
            if hooks.iter().any(|(registered, _)| *registered == key) {
                return Ok(Vec::new());
            }
            hooks.push((key, hook.clone()));
        }
        let open: Vec<Self> = layout.chats.lock().unwrap().values().cloned().collect();
        let mut failures = Vec::new();
        for chat in &open {
            if let Err(error) = hook(chat) {
                failures.push(ChatOpenFailure {
                    chat: chat.0.path.clone().unwrap_or_default(),
                    error,
                });
            }
        }
        Ok(failures)
    }
    pub fn chat_path(&self, session: &str) -> Result<Option<PathBuf>> {
        match self.scoped(session)? {
            Some(store) => Ok(store.0.path.clone()),
            None => Ok(self.0.path.clone()),
        }
    }
    pub fn catalog_rebuild(&self) -> Result<usize> {
        self.catalog_rebuild_mode(true)
    }
    fn catalog_rebuild_mode(&self, force_archived: bool) -> Result<usize> {
        // Standalone catalog repair must not compete with an attached buffered
        // projection transaction or leave that transaction on an old snapshot.
        self.flush_serving()?;
        let layout = self
            .0
            .layout
            .as_ref()
            .context("Catalog requires a directory store")?;
        self.recover_archives()?;
        let mut records = Vec::new();
        let mut search_targets: HashMap<String, i64> = {
            let catalog = layout.catalog.lock().unwrap();
            let mut statement = catalog.prepare(
                "SELECT substr(key,15),value FROM metadata WHERE substr(key,1,14)='search_target:'",
            )?;
            let mut targets = HashMap::new();
            for row in statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })? {
                let (session, target) = row?;
                if let Ok(target) = target.parse::<i64>() {
                    targets.insert(session, target);
                }
            }
            targets
        };
        // The catalog workspace is only the legacy fallback. Root chats may
        // carry different authoritative bindings; rebuilding never selects one
        // of those roots as the default for the others.
        self.bound_workspace()?;
        let mut archive_times: HashMap<String, String> = HashMap::new();
        let mut known_archives: HashMap<String, Vec<(Session, String)>> = HashMap::new();
        if !force_archived {
            let catalog = layout.catalog.lock().unwrap();
            // A completed archive is immutable, and its complete session tree
            // was catalogued before the active -> archiving transition. Normal
            // startup keeps those metadata rows without decoding cold files.
            let mut statement=catalog.prepare("SELECT root_id,data,last_activity FROM sessions AS archived WHERE storage_state='archived' AND EXISTS(SELECT 1 FROM sessions AS root WHERE root.id=archived.root_id AND root.storage_state='archived') AND NOT EXISTS(SELECT 1 FROM sessions AS mixed WHERE mixed.root_id=archived.root_id AND mixed.storage_state!='archived')")?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let root: String = row.get(0)?;
                let session: Session = serde_json::from_str(&row.get::<_, String>(1)?)?;
                known_archives
                    .entry(root)
                    .or_default()
                    .push((session, row.get(2)?));
            }
        }
        let mut archived_roots = std::collections::HashSet::new();
        let mut root_storage: HashMap<String, Option<RootStoragePolicy>> = HashMap::new();
        for entry in std::fs::read_dir(layout.data_dir.join("chats"))? {
            let mut path = entry?.path();
            let archived = path.to_string_lossy().ends_with(".db.zst");
            if archived {
                path = path.with_extension("");
                if path.is_file() {
                    continue;
                }
            } else if path.extension().is_none_or(|extension| extension != "db") {
                continue;
            }
            let root = path
                .file_stem()
                .context("Chat filename missing")?
                .to_string_lossy()
                .into_owned();
            if archived && let Some(sessions) = known_archives.remove(&root) {
                let projected: Option<String> = layout
                    .catalog
                    .lock()
                    .unwrap()
                    .query_row(
                        "SELECT value FROM metadata WHERE key=?1",
                        [format!("root_storage:{root}")],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(value) = projected
                    && let Ok(policy) = serde_json::from_str::<Option<RootStoragePolicy>>(&value)
                    && policy
                        .as_ref()
                        .is_none_or(|policy| policy.validate().is_ok())
                {
                    root_storage.insert(root.clone(), policy);
                    archived_roots.insert(root.clone());
                    records.extend(
                        sessions
                            .into_iter()
                            .map(|(session, activity)| (root.clone(), session, activity)),
                    );
                    continue;
                }
            }
            // A malformed workspace binding rejects only its own chat: the
            // rebuild skips it with a notice and never rewrites the file.
            let binding_error = |conn: &Connection| -> Result<Option<String>> {
                let mut bindings = conn.prepare(
                    "SELECT key,value FROM metadata WHERE key='workspace' OR substr(key,1,15)='root_workspace:'"
                )?;
                for binding in bindings.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })? {
                    let (key, value) = binding?;
                    if key != "workspace" && key != format!("root_workspace:{root}") {
                        return Ok(Some(format!("binding key {key} names another root")));
                    }
                    if let Err(error) = Self::workspace_binding(&value) {
                        return Ok(Some(format!("{error:#}")));
                    }
                }
                Ok(None)
            };
            let skip = |reason: String| {
                tracing::warn!(
                    "Skipping chat {root}: Malformed root workspace binding ({reason}); the chat file is left unchanged"
                );
            };
            let inspect = |conn: &Connection| -> Result<(Vec<Session>, Option<RootStoragePolicy>)> {
                let mut statement = conn.prepare("SELECT data FROM sessions ORDER BY rowid")?;
                let sessions = statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .map(|row| Ok(serde_json::from_str(&row?)?))
                    .collect::<Result<Vec<Session>>>()?;
                let mut storage = conn.prepare(
                    "SELECT key,value FROM metadata WHERE substr(key,1,13)='root_storage:'",
                )?;
                let mut policy = None;
                for row in storage.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })? {
                    let (key, value) = row?;
                    anyhow::ensure!(
                        key == format!("root_storage:{root}"),
                        "Malformed root storage policy binding"
                    );
                    let saved = serde_json::from_str::<RootStoragePolicy>(&value)
                        .context("Malformed root storage policy")?;
                    saved.validate()?;
                    policy = Some(saved);
                }
                Ok((sessions, policy))
            };
            let mut archived_record = None;
            let mut active_record = None;
            let (sessions, policy) = if archived {
                let record = open_record(&path, true, true)?;
                if let Some(reason) = binding_error(&record)? {
                    skip(reason);
                    continue;
                }
                archived_roots.insert(root.clone());
                archived_record = Some(record);
                if let Some(time) = archived_record
                    .as_ref()
                    .unwrap()
                    .query_row(
                        "SELECT value FROM metadata WHERE key='archived_at'",
                        [],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                {
                    archive_times.insert(root.clone(), time);
                }
                inspect(archived_record.as_ref().unwrap())?
            } else {
                let record = self.open_chat(&root)?;
                if let Some(reason) = record.read(binding_error)? {
                    skip(reason);
                    layout.chats.lock().unwrap().remove(&root);
                    continue;
                }
                active_record = Some(record);
                active_record.as_ref().unwrap().read(inspect)?
            };
            root_storage.insert(root.clone(), policy);
            for session in sessions {
                let target = |conn: &Connection| -> Result<i64> {
                    Ok(conn.query_row(
                        "SELECT COALESCE(MAX(seq),0) FROM messages WHERE session_id=?1",
                        [&session.id],
                        |row| row.get(0),
                    )?)
                };
                let maximum = if archived {
                    target(archived_record.as_ref().unwrap())?
                } else {
                    active_record.as_ref().unwrap().read(target)?
                };
                search_targets.insert(session.id.clone(), maximum);
                let activity = if archived {
                    Self::record_activity_connection(archived_record.as_ref().unwrap(), &session)?
                } else {
                    active_record.as_ref().unwrap().record_activity(&session)?
                };
                records.push((root.clone(), session, activity));
            }
        }
        let mut catalog = layout.catalog.lock().unwrap();
        let tx = catalog.transaction()?;
        tx.execute("DELETE FROM sessions", [])?;
        tx.execute(
            "DELETE FROM metadata WHERE substr(key,1,13)='root_storage:'",
            [],
        )?;
        for (root, policy) in root_storage {
            tx.execute(
                "INSERT INTO metadata VALUES(?1,?2)",
                params![
                    format!("root_storage:{root}"),
                    serde_json::to_string(&policy)?
                ],
            )?;
        }
        for (root, time) in archive_times {
            tx.execute(
                "INSERT OR REPLACE INTO metadata VALUES(?1,?2)",
                params![format!("archived_at:{root}"), time],
            )?;
        }
        tx.execute(
            "DELETE FROM metadata WHERE substr(key,1,14)='search_target:'",
            [],
        )?;
        for (root, session, activity) in &records {
            Self::catalog_session(&tx, root, session, activity)?;
            if let Some(target) = search_targets.get(&session.id) {
                tx.execute(
                    "INSERT INTO metadata VALUES(?1,?2)",
                    params![format!("search_target:{}", session.id), target.to_string()],
                )?;
            }
            if archived_roots.contains(root) {
                tx.execute(
                    "UPDATE sessions SET storage_state='archived' WHERE id=?1",
                    [&session.id],
                )?;
            }
        }
        tx.execute("DELETE FROM metadata WHERE substr(key,1,12)='archived_at:' AND NOT EXISTS(SELECT 1 FROM sessions WHERE sessions.id=substr(metadata.key,13) AND sessions.id=sessions.root_id)", [])?;
        tx.commit()?;
        layout.catalog_transactions.fetch_add(1, Ordering::Relaxed);
        drop(catalog);
        layout.pending_catalog.lock().unwrap().clear();
        // The rebuilt catalog is authoritative again; drop every resolved placement.
        layout.session_roots.lock().unwrap().clear();
        // Session metadata in serving supports compatibility without owning content.
        let removed_events = self.write(|tx| {
            tx.execute("DELETE FROM sessions", [])?;
            for (_, session, _) in &records {
                tx.execute(
                    "INSERT INTO sessions VALUES(?1,?2)",
                    params![session.id, serde_json::to_string(session)?],
                )?;
                if let Some(target) = search_targets.get(&session.id) {
                    // Do this before later appends can advance a previously
                    // complete watermark past an index tail lost on interruption.
                    tx.execute("UPDATE search_progress SET complete=0 WHERE session_id=?1 AND through_seq<?2", params![session.id,target])?;
                }
            }
            let orphaned_search: Vec<String> = {
                let mut statement=tx.prepare("SELECT DISTINCT session_id FROM search_rows WHERE NOT EXISTS(SELECT 1 FROM sessions WHERE sessions.id=search_rows.session_id)")?;
                statement.query_map([],|row|row.get(0))?.collect::<rusqlite::Result<_>>()?
            };
            Self::remove_search_sessions(tx,&orphaned_search)?;
            let removed: Option<i64> = tx.query_row("SELECT MAX(seq) FROM events WHERE NOT EXISTS(SELECT 1 FROM sessions WHERE sessions.id=events.session_id)", [], |row| row.get(0))?;
            for table in ["events", "context_views", "runs", "inputs", "approvals", "telemetry"] {
                tx.execute(&format!("DELETE FROM {table} WHERE session_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM sessions WHERE sessions.id={table}.session_id)"), [])?;
            }
            if let Some(removed) = removed {
                crate::event_log::remember_last(tx, removed as u64)?;
                let previous: Option<String> = tx.query_row("SELECT value FROM metadata WHERE key='events_pruned_through'", [], |row| row.get(0)).optional()?;
                let pruned = removed.max(previous.and_then(|value| value.parse().ok()).unwrap_or(0));
                tx.execute("INSERT OR REPLACE INTO metadata VALUES('events_pruned_through',?1)", [pruned.to_string()])?;
                return Ok(Some(pruned as u64));
            }
            Ok(None)
        })?;
        if let Some(removed) = removed_events {
            self.runtime().log.mark_pruned(removed);
        }
        Ok(records.len())
    }
    fn record_activity(&self, session: &Session) -> Result<String> {
        self.read(|conn| Self::record_activity_connection(conn, session))
    }
    fn record_activity_connection(conn: &Connection, session: &Session) -> Result<String> {
        let mut activity = session.updated_at.clone();
        let has_headers: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='message_headers')",
            [],
            |row| row.get(0),
        )?;
        let created: Option<String> = if has_headers {
            // Activity is metadata: a large tool body must not be decompressed
            // again after every record step just to obtain its timestamp.
            conn.query_row("SELECT created_at FROM message_headers WHERE session_id=?1 ORDER BY message_seq DESC LIMIT 1", [&session.id], |row| row.get(0)).optional()?
        } else {
            // Read-only phase-B records are scanned before their header migration.
            let message: Option<String> = conn
                .query_row(
                    "SELECT data FROM messages WHERE session_id=?1 ORDER BY seq DESC LIMIT 1",
                    [&session.id],
                    |row| row.get(0),
                )
                .optional()?;
            message
                .map(|message| -> Result<_> {
                    Ok(Self::decode_message_body(conn, &message)?["created_at"]
                        .as_str()
                        .map(str::to_owned))
                })
                .transpose()?
                .flatten()
        };
        if let Some(created) = created {
            activity = activity.max(created);
        }
        let run: Option<String> = conn
            .query_row(
                "SELECT data FROM runs WHERE session_id=?1 ORDER BY rowid DESC LIMIT 1",
                [&session.id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(run) = run {
            let run: serde_json::Value = serde_json::from_str(&run)?;
            if let Some(created) = run["ended_at"]
                .as_str()
                .or_else(|| run["started_at"].as_str())
            {
                activity = activity.max(created.to_owned());
            }
        }
        Ok(activity)
    }
    pub(super) fn catalog_session(
        conn: &Connection,
        root: &str,
        session: &Session,
        activity: &str,
    ) -> Result<()> {
        Self::catalog_session_to(conn, "sessions", root, session, activity)
    }
    pub(super) fn catalog_session_to(
        conn: &Connection,
        table: &str,
        root: &str,
        session: &Session,
        activity: &str,
    ) -> Result<()> {
        anyhow::ensure!(
            matches!(table, "sessions" | "catalog_projection.sessions"),
            "Invalid catalog destination"
        );
        let sql = format!(
            "INSERT INTO {table}(id,parent_id,root_id,title,last_activity,created_at,file,storage_state,data) VALUES(?1,?2,?3,?4,?5,?6,?7,'active',?8) ON CONFLICT(id) DO UPDATE SET parent_id=excluded.parent_id,root_id=excluded.root_id,title=excluded.title,last_activity=excluded.last_activity,created_at=excluded.created_at,file=excluded.file,storage_state='active',data=excluded.data WHERE sessions.data!=excluded.data OR sessions.last_activity!=excluded.last_activity"
        );
        conn.execute(
            &sql,
            params![
                session.id,
                session.parent_session_id,
                root,
                session.title,
                activity,
                session.created_at,
                format!("chats/{root}.db"),
                serde_json::to_string(session)?
            ],
        )?;
        Ok(())
    }
    pub(super) fn refresh_catalog_best_effort(&self) {
        let Some(host) = self.host() else {
            return;
        };
        let Some(layout) = &host.0.layout else {
            return;
        };
        let Some(root) = self
            .0
            .path
            .as_ref()
            .and_then(|path| path.file_stem())
            .map(|root| root.to_string_lossy().into_owned())
        else {
            return;
        };
        let result = (|| -> Result<()> {
            let sessions = self.list_sessions()?;
            let records = sessions
                .iter()
                .map(|session| Ok((session, self.record_activity(session)?)))
                .collect::<Result<Vec<_>>>()?;
            let mut pending = layout.pending_catalog.lock().unwrap();
            for (session, activity) in records {
                pending.insert(
                    session.id.clone(),
                    (root.clone(), session.clone(), activity),
                );
            }
            Ok(())
        })();
        // Catalog is explicitly rebuildable; it never changes success of record commits.
        let _ = result;
    }
    pub(super) fn layout_bind_workspace(&self, path: &Path) -> Result<()> {
        let root = path.canonicalize()?.to_string_lossy().into_owned();
        let layout = self.0.layout.as_ref().unwrap();
        let existing: Option<String> = layout
            .catalog
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM metadata WHERE key='workspace'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            anyhow::ensure!(
                existing == root,
                "Database belongs to another workspace: {existing}"
            );
            return Ok(());
        }
        let mut catalog = layout.catalog.lock().unwrap();
        let tx = catalog.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO metadata VALUES('workspace',?1)",
            [&root],
        )?;
        tx.commit()?;
        layout.catalog_transactions.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    pub(super) fn layout_session_workspace(&self, session: &str) -> Result<Option<PathBuf>> {
        let layout = self.0.layout.as_ref().unwrap();
        let workspace =
            open_session_record(&layout.catalog, &layout.data_dir, session, |root, path| {
                let cached = layout.chats.lock().unwrap().get(root).cloned();
                if let Some(chat) = cached {
                    return chat.read(|conn| {
                        Self::session_workspace_connection(conn, session, Some(root))
                    });
                }
                let record = open_record(path, true, false)?;
                Self::session_workspace_connection(&record, session, Some(root))
            })?
            .context("Unknown session or missing chat file")?
            .0;
        match workspace {
            Some(workspace) => Ok(Some(workspace)),
            None => self.bound_workspace(),
        }
    }
    pub(super) fn layout_session_storage_policy(
        &self,
        session: &str,
    ) -> Result<Option<RootStoragePolicy>> {
        let layout = self.0.layout.as_ref().unwrap();
        // This immutable projection is rebuilt from root-chat metadata. It also
        // lets retention inspect archived roots without decoding their bodies.
        let projected: Option<String> = layout.catalog.lock().unwrap().query_row(
            "SELECT m.value FROM sessions s LEFT JOIN metadata m ON m.key='root_storage:'||s.root_id WHERE s.id=?1", [session], |row| row.get(0)
        ).optional()?.flatten();
        if let Some(value) = projected
            && let Ok(policy) = serde_json::from_str::<Option<RootStoragePolicy>>(&value)
            && policy
                .as_ref()
                .is_none_or(|policy| policy.validate().is_ok())
        {
            return Ok(policy);
        }
        Ok(
            open_session_record(&layout.catalog, &layout.data_dir, session, |root, path| {
                let cached = layout.chats.lock().unwrap().get(root).cloned();
                if let Some(chat) = cached {
                    return chat
                        .read(|conn| Self::session_storage_connection(conn, session, Some(root)));
                }
                let record = open_record(path, true, false)?;
                Self::session_storage_connection(&record, session, Some(root))
            })?
            .context("Unknown session or missing chat file")?
            .0,
        )
    }
    pub(super) fn layout_sessions(&self) -> Result<Vec<Session>> {
        let layout = self.0.layout.as_ref().unwrap();
        let mut sessions: HashMap<String, (Session, String)> = {
            let catalog = layout.catalog.lock().unwrap();
            let mut stmt = catalog.prepare("SELECT data,last_activity FROM sessions")?;
            stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (data, activity) = row?;
                let session: Session = serde_json::from_str(&data)?;
                Ok((session.id.clone(), (session, activity)))
            })
            .collect::<Result<_>>()?
        };
        for (_, session, activity) in layout.pending_catalog.lock().unwrap().values() {
            sessions.insert(session.id.clone(), (session.clone(), activity.clone()));
        }
        let mut sessions: Vec<_> = sessions.into_values().collect();
        sessions.sort_by(|(left, la), (right, ra)| ra.cmp(la).then_with(|| left.id.cmp(&right.id)));
        Ok(sessions.into_iter().map(|(session, _)| session).collect())
    }
    pub(super) fn layout_summaries(
        &self,
        limit: usize,
        visible_only: bool,
    ) -> Result<Vec<SessionRecord>> {
        // A sidebar read must not reactivate cold chats. Share each temporary
        // read-only unpack across its root and child rows for this bounded page.
        let layout = self.0.layout.as_ref().unwrap();
        let mut archives: HashMap<String, RecordConnection> = HashMap::new();
        self.list_sessions()?
            .into_iter()
            .filter(|session| !visible_only || !session.hidden)
            .take(limit)
            .map(|session| {
                if self.active_session_metadata(&session.id)?.is_none() {
                    let (root, _) = open_session_record(
                        &layout.catalog,
                        &layout.data_dir,
                        &session.id,
                        |root, path| {
                            if let std::collections::hash_map::Entry::Vacant(entry) =
                                archives.entry(root.to_owned())
                            {
                                entry.insert(open_record(path, true, true)?);
                            }
                            Ok(root.to_owned())
                        },
                    )?
                    .context("Summary record was removed")?;
                    return self.cold_session_summary(archives.get(&root).unwrap(), session);
                }
                let snap = self.snapshot(&session.id, 1, 1, 0)?;
                Ok(SessionRecord {
                    session,
                    message_count: snap.message_count,
                    last_message: snap.messages.into_iter().next(),
                    latest_run: snap.runs.into_iter().next(),
                    pending_input_count: snap.pending_input_count,
                })
            })
            .collect()
    }
    /// Read summary fields through the ordinary decoder without changing a
    /// record's placement. Finished run metadata may already live in serving.
    fn cold_session_summary(&self, record: &Connection, session: Session) -> Result<SessionRecord> {
        let count = |table: &str, extra: &str| -> Result<u64> {
            Ok(record.query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE session_id=?1 {extra}"),
                [&session.id],
                |row| row.get::<_, i64>(0),
            )? as u64)
        };
        let data: Option<String> = record
            .query_row(
                "SELECT data FROM messages WHERE session_id=?1 ORDER BY seq DESC LIMIT 1",
                [&session.id],
                |row| row.get(0),
            )
            .optional()?;
        let last_message = data
            .map(|data| Self::message_from_data(record, &data))
            .transpose()?;
        let latest = |conn: &Connection| -> Result<Option<String>> {
            Ok(conn.query_row("SELECT data FROM runs WHERE session_id=?1 ORDER BY json_extract(data,'$.started_at') DESC,id DESC LIMIT 1", [&session.id], |row| row.get(0)).optional()?)
        };
        let mut runs = [latest(record)?, self.read(latest)?]
            .into_iter()
            .flatten()
            .map(|data| Self::run_from_data(record, &data))
            .collect::<Result<Vec<_>>>()?;
        runs.sort_by(|a, b| a.started_at.cmp(&b.started_at).then(a.id.cmp(&b.id)));
        runs.dedup_by(|a, b| a.id == b.id);
        Ok(SessionRecord {
            message_count: count("messages", "")?,
            pending_input_count: count("inputs", "AND state='pending'")?,
            last_message,
            latest_run: runs.pop(),
            session,
        })
    }
    pub(crate) fn find_record(&self, table: &str, key: &str) -> Result<Self> {
        if !self.is_layout() {
            return Ok(self.clone());
        }
        anyhow::ensure!(
            ["runs", "effects", "approvals"].contains(&table),
            "Invalid record table"
        );
        if table != "effects" {
            let session: Option<String> = self.read(|conn| {
                Ok(conn
                    .query_row(
                        &format!("SELECT session_id FROM {table} WHERE id=?1"),
                        [key],
                        |row| row.get(0),
                    )
                    .optional()?)
            })?;
            if let Some(session) = session {
                return Ok(self.scoped(&session)?.unwrap());
            }
        }
        for session in self.list_sessions()? {
            let chat = self.scoped(&session.id)?.unwrap();
            let exists = chat.read(|conn| {
                Ok(conn.query_row(
                    &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE id=?1)"),
                    [key],
                    |row| row.get::<_, bool>(0),
                )?)
            })?;
            if exists {
                return Ok(chat);
            }
        }
        // Archival commits serving first, then removes the canonical operation
        // row. It may have crossed both earlier lookups while this scan ran.
        if table != "effects" {
            let session: Option<String> = self.read(|conn| {
                Ok(conn
                    .query_row(
                        &format!("SELECT session_id FROM {table} WHERE id=?1"),
                        [key],
                        |row| row.get(0),
                    )
                    .optional()?)
            })?;
            if let Some(session) = session {
                return Ok(self.scoped(&session)?.unwrap());
            }
        }
        bail!("Unknown {table} record: {key}")
    }
    pub(crate) fn run_row(&self, run_id: &str) -> Result<(String, Option<String>)> {
        let row = self.read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT data,result_message_id FROM runs WHERE id=?1",
                    [run_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?)
        })?;
        if let Some(row) = row {
            return Ok(row);
        }
        if let Some(host) = self.host() {
            return host.read(|conn| {
                Ok(conn.query_row(
                    "SELECT data,result_message_id FROM runs WHERE id=?1",
                    [run_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?)
            });
        }
        bail!("Unknown run: {run_id}")
    }
    pub(crate) fn approval_record(&self, approval: &str) -> Result<crate::ApprovalRecord> {
        if self.is_layout() {
            return self
                .find_record("approvals", approval)?
                .approval_record(approval);
        }
        self.read(|conn| {
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM approvals WHERE id=?1)",
                [approval],
                |row| row.get(0),
            )?;
            if exists {
                return crate::approvals::read_record(conn, approval);
            }
            let host = self.host().context("Unknown approval")?;
            host.read(|projection| crate::approvals::read_record_from(conn, projection, approval))
        })
    }
    pub(crate) fn archived_approval_state(&self, approval: &str) -> Result<Option<String>> {
        let host = if self.is_layout() {
            Some(self.clone())
        } else {
            self.host()
        };
        match host {
            Some(host) => host.read(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT state FROM approvals WHERE id=?1",
                        [approval],
                        |row| row.get(0),
                    )
                    .optional()?)
            }),
            None => Ok(None),
        }
    }
    pub(crate) fn archive_operations(&self) -> Result<()> {
        let Some(host) = self.host() else {
            return Ok(());
        };
        let runs: Vec<String> = self.read(|conn| {
            let mut stmt =
                conn.prepare("SELECT id FROM runs WHERE status NOT IN ('running','interrupted','waiting_for_quota')")?;
            Ok(stmt
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?)
        })?;
        let approvals: Vec<String> = self.read(|conn| {
            let mut stmt = conn.prepare("SELECT id FROM approvals WHERE state!='pending'")?;
            Ok(stmt
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?)
        })?;
        let inputs: Vec<String> = self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id FROM inputs WHERE state NOT IN ('pending','reminder_pending')",
            )?;
            Ok(stmt
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?)
        })?;
        if runs.is_empty() && approvals.is_empty() && inputs.is_empty() {
            return Ok(());
        }
        self.read(|record| host.write(|projection| {
            for (table, ids) in [("runs", &runs), ("approvals", &approvals)] {
                for key in ids { Self::copy_rows(record, projection, table, " WHERE id=?", std::slice::from_ref(key))?; }
            }
            for key in &inputs {
                let values = record.query_row("SELECT seq,id,session_id,kind,text,state,origin,message_id,content_blob,content_format FROM inputs WHERE id=?1", [key], |row| (0..10).map(|index| row.get::<_, rusqlite::types::Value>(index)).collect::<rusqlite::Result<Vec<_>>>())?;
                let mut values = values; values.push(now().into());
                projection.execute("INSERT INTO inputs(source_seq,id,session_id,kind,text,state,origin,message_id,content_blob,content_format,resolved_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11) ON CONFLICT(id) DO NOTHING", rusqlite::params_from_iter(values))?;
            }
            Ok(())
        }))?;
        self.write(|tx| {
            for key in &runs {
                tx.execute(
                    "DELETE FROM runs WHERE id=?1 AND status NOT IN ('running','interrupted','waiting_for_quota')",
                    [key],
                )?;
            }
            for key in &approvals {
                tx.execute(
                    "DELETE FROM approvals WHERE id=?1 AND state!='pending'",
                    [key],
                )?;
            }
            for key in &inputs {
                tx.execute("DELETE FROM inputs WHERE id=?1 AND state NOT IN ('pending','reminder_pending')", [key])?;
            }
            tx.execute("UPDATE effects SET detail=NULL WHERE state='completed'", [])?;
            Ok(())
        })
    }
    pub(super) fn split_context_telemetry(
        &self,
        conn: &Connection,
    ) -> Result<Vec<(String, String, String, String)>> {
        if self.host().is_none() {
            return Ok(Vec::new());
        }
        let mut telemetry = Vec::new();
        for table in [
            "context_checkpoints",
            "context_chapters",
            "context_episodes",
            "context_subject_chapters",
            "context_decisions",
        ] {
            let mut stmt = conn.prepare(&format!("SELECT id,session_id,data FROM {table} WHERE json_type(data,'$.usage') NOT IN ('null') OR json_type(data,'$.reasoning') NOT IN ('null')"))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (key, session, data) in rows {
                let mut value: serde_json::Value = serde_json::from_str(&data)?;
                let usage = value.as_object_mut().unwrap().remove("usage");
                let reasoning = value.as_object_mut().unwrap().remove("reasoning");
                let metadata =
                    serde_json::json!({"record_id":key,"usage":usage,"reasoning":reasoning});
                telemetry.push((
                    session,
                    value["created_at"].as_str().unwrap_or(&now()).to_owned(),
                    table.to_owned(),
                    metadata.to_string(),
                ));
                conn.execute(
                    &format!("UPDATE {table} SET data=?2 WHERE id=?1"),
                    params![key, value.to_string()],
                )?;
            }
        }
        Ok(telemetry)
    }
    pub(super) fn save_telemetry_best_effort(&self, rows: &[(String, String, String, String)]) {
        if rows.is_empty() {
            return;
        }
        if let Some(host) = self.host() {
            let _ = host.write(|tx| {
                for (session, created, kind, data) in rows {
                    let key = serde_json::from_str::<serde_json::Value>(data)?["record_id"].as_str().context("Telemetry record ID missing")?.to_owned();
                    tx.execute("DELETE FROM telemetry WHERE session_id=?1 AND kind=?2 AND json_extract(data,'$.record_id')=?3", params![session, kind, key])?;
                    tx.execute("INSERT INTO telemetry(session_id,created_at,kind,data) VALUES(?1,?2,?3,?4)", params![session, created, kind, data])?;
                }
                Ok(())
            });
        }
    }
    pub(super) fn hydrate_context_telemetry(&self, data: &str) -> Result<String> {
        let Some(host) = self.host() else {
            return Ok(data.into());
        };
        let mut value: serde_json::Value = serde_json::from_str(data)?;
        let key = value["id"].as_str().context("Context record ID missing")?;
        let session = value["session_id"]
            .as_str()
            .context("Context session ID missing")?;
        let metadata: Option<String> = host.read(|conn| Ok(conn.query_row("SELECT data FROM telemetry WHERE session_id=?1 AND json_extract(data,'$.record_id')=?2 ORDER BY id DESC LIMIT 1", params![session, key], |row| row.get(0)).optional()?))?;
        if let Some(metadata) = metadata {
            let metadata: serde_json::Value = serde_json::from_str(&metadata)?;
            for field in ["usage", "reasoning"] {
                if !metadata[field].is_null() {
                    value[field] = metadata[field].clone();
                }
            }
        }
        Ok(value.to_string())
    }
    pub(crate) fn all_runs(&self, session: &str) -> Result<Vec<Run>> {
        let mut rows = self.read(|conn| {
            let mut stmt =
                conn.prepare("SELECT data FROM runs WHERE session_id=?1 ORDER BY rowid")?;
            Ok(stmt
                .query_map([session], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        if let Some(host) = self.host() {
            rows.extend(host.read(|conn| {
                let mut stmt =
                    conn.prepare("SELECT data FROM runs WHERE session_id=?1 ORDER BY rowid")?;
                Ok(stmt
                    .query_map([session], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })?);
        }
        let mut runs = self.read(|conn| {
            rows.into_iter()
                .map(|data| Self::run_from_data(conn, &data))
                .collect::<Result<Vec<_>>>()
        })?;
        runs.sort_by(|a, b| a.started_at.cmp(&b.started_at).then(a.id.cmp(&b.id)));
        runs.dedup_by(|a, b| a.id == b.id);
        Ok(runs)
    }
    pub(crate) fn input_event_row(&self, session: &str, input: &str) -> Result<InputEventRow> {
        let sql = "SELECT text,message_id,content_blob,content_format FROM inputs WHERE session_id=?1 AND id=?2";
        let row = self.read(|conn| {
            Ok(conn
                .query_row(sql, params![session, input], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })
                .optional()?)
        })?;
        if let Some(row) = row {
            return Ok(row);
        }
        let host = self.host().context("Unknown accepted input")?;
        host.read(|conn| {
            Ok(conn.query_row(sql, params![session, input], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?)
        })
    }
    pub(super) fn layout_append_batch(&self, messages: &[Message]) -> Result<()> {
        let mut groups: HashMap<PathBuf, (Self, Vec<Message>)> = HashMap::new();
        for message in messages {
            let chat = self.scoped(&message.session_id)?.unwrap();
            groups
                .entry(chat.0.path.clone().unwrap())
                .or_insert_with(|| (chat, Vec::new()))
                .1
                .push(message.clone());
        }
        // Each group is its own authoritative commit; no cross-file transaction.
        for (chat, messages) in groups.into_values() {
            chat.append_batch(&messages)?;
        }
        Ok(())
    }
    pub(super) fn layout_fork(
        &self,
        source_id: &str,
        message_id: Option<&str>,
        title: String,
        admitted: impl FnOnce() -> Result<()>,
        after: impl FnOnce(&Session) -> Result<()>,
    ) -> Result<Session> {
        let runtime = self.runtime();
        let _guard = runtime.fork_guard(source_id)?;
        admitted()?;
        let source = self.scoped(source_id)?.unwrap();
        let dispatched = source.read(|conn| {
            Ok(conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM effects WHERE session_id=?1 AND state='dispatched')",
                [source_id],
                |row| row.get::<_, bool>(0),
            )?)
        })?;
        anyhow::ensure!(!dispatched, "Cannot fork with active background effects");
        let history = Self::fork_history(source.messages(source_id)?, message_id)?;
        let mut session = self.create_session_with_parent(
            title,
            source.session(source_id)?.context_strategy,
            Some(source_id),
        )?;
        session.parent_cutoff = Some(history.len() as i64);
        source.write(|tx| {
            tx.execute(
                "UPDATE sessions SET data=?2 WHERE id=?1",
                params![session.id, serde_json::to_string(&session)?],
            )?;
            for mut message in history {
                let original_message_id = message.id.clone();
                message.id = id();
                message.session_id = session.id.clone();
                Self::insert_message(tx, &message)?;
                Self::copy_reminder_metadata_tx(
                    tx,
                    source_id,
                    &original_message_id,
                    &session.id,
                    &message.id,
                )?;
            }
            Ok(())
        })?;
        after(&session)?;
        Ok(session)
    }
    pub fn serving_wipe(&self) -> Result<()> {
        anyhow::ensure!(self.is_layout(), "Serving wipe requires a directory store");
        self.runtime().log.reset_serving(|last| {
            let path = self.0.path.as_ref().unwrap();
            let mut connection = self.0.connection.lock().unwrap();
            // Drop the old connection before removing files (also on Windows).
            let old = std::mem::replace(&mut *connection, Connection::open_in_memory()?);
            drop(old);
            for suffix in ["", "-wal", "-shm"] {
                let file = PathBuf::from(format!("{}{suffix}", path.display()));
                if file.exists() {
                    std::fs::remove_file(file)?;
                }
            }
            *connection = projection(path, 0x42504232, SERVING_SCHEMA)?;
            let tx = connection.transaction()?;
            tx.execute(
                "INSERT INTO metadata VALUES('events_last_seq',?1)",
                [last.to_string()],
            )?;
            tx.execute(
                "INSERT INTO metadata VALUES('events_pruned_through',?1)",
                [last.to_string()],
            )?;
            tx.commit()?;
            Ok(())
        })
    }
    pub fn housekeeping(&self) -> Result<()> {
        let Some(layout) = &self.0.layout else {
            return Ok(());
        };
        let instant = chrono::Utc::now();
        let host_telemetry_cutoff = instant
            .checked_sub_signed(chrono::Duration::days(i64::from(
                layout.options.telemetry_retention_days,
            )))
            .context("Telemetry retention is outside the supported date range")?
            .to_rfc3339();
        let cutoffs = {
            let catalog = layout.catalog.lock().unwrap();
            let mut statement = catalog.prepare("SELECT s.id,m.value FROM sessions s LEFT JOIN metadata m ON m.key='root_storage:'||s.root_id")?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })?
                .map(|row| -> Result<_> {
                    let (session, value) = row?;
                    let policy = value
                        .map(|value| serde_json::from_str::<Option<RootStoragePolicy>>(&value))
                        .transpose()?
                        .flatten();
                    let options = policy.map_or(layout.options, |policy| policy.store);
                    let events = instant
                        .checked_sub_signed(chrono::Duration::hours(i64::from(
                            options.finished_event_retention_hours,
                        )))
                        .context("Finished event retention is outside the supported date range")?
                        .to_rfc3339();
                    let telemetry = instant
                        .checked_sub_signed(chrono::Duration::days(i64::from(
                            options.telemetry_retention_days,
                        )))
                        .context("Telemetry retention is outside the supported date range")?
                        .to_rfc3339();
                    Ok((session, events, telemetry))
                })
                .collect::<Result<Vec<_>>>()?
        };
        let pruned = self.write(|tx| {
            // Cutoffs are a transient serving view. All roots are pruned in one
            // serving transaction, and canonical chats remain untouched here.
            tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS root_housekeeping(session_id TEXT PRIMARY KEY,event_cutoff TEXT NOT NULL,telemetry_cutoff TEXT NOT NULL) WITHOUT ROWID; DELETE FROM root_housekeeping;")?;
            {
                let mut insert = tx.prepare_cached("INSERT INTO root_housekeeping VALUES(?1,?2,?3)")?;
                for (session, events, telemetry) in &cutoffs {
                    insert.execute(params![session, events, telemetry])?;
                }
            }
            let through: Option<i64> = tx.query_row("SELECT MAX(e.seq) FROM events e JOIN root_housekeeping p ON p.session_id=e.session_id JOIN runs r ON r.id=e.run_id WHERE e.created_at<p.event_cutoff AND r.status NOT IN ('running','waiting_for_quota') AND COALESCE(json_extract(r.data,'$.ended_at'),'')<p.event_cutoff", [], |row| row.get(0))?;
            let previous: Option<String> = tx.query_row("SELECT value FROM metadata WHERE key='events_pruned_through'", [], |row| row.get(0)).optional()?;
            let through = through.map(|through| through.max(previous.and_then(|value| value.parse().ok()).unwrap_or(0)));
            if let Some(through) = through {
                // Retain active-run events even inside the pruned range. A client
                // behind its highest removed sequence must take a new snapshot.
                tx.execute("DELETE FROM events WHERE seq<=?1 AND EXISTS(SELECT 1 FROM root_housekeeping p JOIN runs r ON r.id=events.run_id WHERE p.session_id=events.session_id AND events.created_at<p.event_cutoff AND r.status NOT IN ('running','waiting_for_quota') AND COALESCE(json_extract(r.data,'$.ended_at'),'')<p.event_cutoff)", [through])?;
                tx.execute("INSERT OR REPLACE INTO metadata VALUES('events_pruned_through',?1)", [through.to_string()])?;
            }
            // Finished run metadata is telemetry (including usage/timings), so
            // keep it for the telemetry window after its event journal expires.
            tx.execute("DELETE FROM runs WHERE status NOT IN ('running','waiting_for_quota') AND EXISTS(SELECT 1 FROM root_housekeeping p WHERE p.session_id=runs.session_id AND COALESCE(json_extract(runs.data,'$.ended_at'),'')<p.telemetry_cutoff) AND NOT EXISTS(SELECT 1 FROM events WHERE events.run_id=runs.id)", [])?;
            tx.execute("DELETE FROM approvals WHERE state!='pending' AND EXISTS(SELECT 1 FROM root_housekeeping p WHERE p.session_id=approvals.session_id AND approvals.decided_at<p.event_cutoff) AND NOT EXISTS(SELECT 1 FROM events WHERE events.run_id=approvals.run_id)", [])?;
            tx.execute("DELETE FROM inputs WHERE state NOT IN ('pending','reminder_pending') AND EXISTS(SELECT 1 FROM root_housekeeping p WHERE p.session_id=inputs.session_id AND inputs.resolved_at<p.event_cutoff) AND NOT EXISTS(SELECT 1 FROM events WHERE json_extract(events.data,'$._content_ref.input_id')=inputs.id)", [])?;
            // Host-only telemetry has no root policy; preserve its legacy bound.
            super::usage::prompt_log::prune(tx, &host_telemetry_cutoff)?;
            Ok(through)
        })?;
        if let Some(pruned) = pruned {
            let last = self.runtime().log.last();
            self.runtime().log.seed(last, pruned as u64);
        }
        let chats: Vec<_> = layout.chats.lock().unwrap().values().cloned().collect();
        for chat in chats {
            for session in chat.list_sessions()? {
                chat.compress_cold_messages(&session.id)?;
            }
        }
        self.flush_serving()?;
        self.archive_idle_chats()?;
        Ok(())
    }
    fn set_chat_state(&self, root: &str, state: &str) -> Result<()> {
        self.set_chat_state_targets(root, state, &[], None)
    }
    fn set_chat_state_targets(
        &self,
        root: &str,
        state: &str,
        targets: &[(String, i64)],
        archived_at: Option<&str>,
    ) -> Result<()> {
        let layout = self
            .0
            .layout
            .as_ref()
            .context("Archive requires a directory store")?;
        let mut catalog = layout.catalog.lock().unwrap();
        let tx = catalog.transaction()?;
        tx.execute(
            "UPDATE sessions SET storage_state=?1 WHERE root_id=?2",
            params![state, root],
        )?;
        if let Some(time) = archived_at {
            tx.execute(
                "INSERT OR REPLACE INTO metadata VALUES(?1,?2)",
                params![format!("archived_at:{root}"), time],
            )?;
        }
        for (session, target) in targets {
            // The archive transition is already FULL. Its canonical sequence
            // receipt survives a NORMAL search projection rollback without
            // requiring known packed chats to be decoded on every startup.
            tx.execute(
                "INSERT OR REPLACE INTO metadata VALUES(?1,?2)",
                params![format!("search_target:{session}"), target.to_string()],
            )?;
        }
        tx.commit()?;
        layout.catalog_transactions.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    fn restore_chat(&self, root: &str, path: &Path) -> Result<()> {
        self.flush_serving()?;
        let archive = cold::archive_path(path);
        let part = cold::staged_path(path, "restore-part");
        if part.exists() {
            std::fs::remove_file(&part)?;
        }
        if let Err(error) = cold::unpack(&archive, &part) {
            let _ = std::fs::remove_file(&part);
            return Err(error);
        }
        let conn = open_record(&part, true, true)?;
        let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        anyhow::ensure!(
            integrity == "ok",
            "Archived chat failed SQLite integrity check"
        );
        drop(conn);
        std::fs::rename(&part, path)?;
        cold::sync_parent(path)?;
        self.set_chat_state(root, "active")?;
        std::fs::remove_file(archive)?;
        cold::sync_parent(path)?;
        Ok(())
    }
    /// Read session metadata without restoring archived storage. Missing sessions
    /// and chat trees whose storage is archiving or archived return `None`.
    pub fn active_session_metadata(&self, session: &str) -> Result<Option<Session>> {
        let host = self.host().unwrap_or_else(|| self.clone());
        let data: Option<String> = if let Some(layout) = &host.0.layout {
            layout
                .catalog
                .lock()
                .unwrap()
                .query_row(
                    "SELECT s.data FROM sessions s JOIN sessions root ON root.id=s.root_id WHERE s.id=?1 AND s.storage_state='active' AND root.storage_state='active'",
                    [session],
                    |row| row.get(0),
                )
                .optional()?
        } else {
            host.read(|conn| {
                Ok(conn
                    .query_row("SELECT data FROM sessions WHERE id=?1", [session], |row| {
                        row.get(0)
                    })
                    .optional()?)
            })?
        };
        data.map(|data| Ok(serde_json::from_str(&data)?))
            .transpose()
    }

    /// Read a bounded page of attachment identifiers and metadata, without blobs.
    /// The prefix is literal; `after_id` is an exclusive keyset cursor.
    pub fn attachment_metadata_page(
        &self,
        session: &str,
        prefix: &str,
        after_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        if let Some(store) = self.scoped(session)? {
            return store.attachment_metadata_page(session, prefix, after_id, limit);
        }
        let limit = i64::try_from(limit).context("Attachment page limit exceeds SQLite range")?;
        self.read(|conn| {
            let mut statement = conn.prepare(
                "SELECT id,metadata FROM attachments WHERE session_id=?1 AND substr(id,1,length(?2))=?2 AND (?3 IS NULL OR id>?3) ORDER BY id LIMIT ?4",
            )?;
            statement
                .query_map(params![session, prefix, after_id, limit], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .map(|row| Ok(row?))
                .collect()
        })
    }

    /// Read the final attachment key and metadata under a literal prefix.
    /// Hosts can allocate ordered keys without loading the complete inventory.
    pub fn attachment_metadata_last(
        &self,
        session: &str,
        prefix: &str,
    ) -> Result<Option<(String, String)>> {
        if let Some(store) = self.scoped(session)? {
            return store.attachment_metadata_last(session, prefix);
        }
        self.read(|conn| {
            Ok(conn.query_row(
                "SELECT id,metadata FROM attachments WHERE session_id=?1 AND substr(id,1,length(?2))=?2 ORDER BY id DESC LIMIT 1",
                params![session, prefix],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional()?)
        })
    }

    /// Archive one complete chat tree after ensuring it has no pending work.
    /// Returns false when another reader or unresolved work keeps it active.
    pub fn archive_chat(&self, session: &str) -> Result<bool> {
        self.flush_serving()?;
        let layout = self
            .0
            .layout
            .as_ref()
            .context("Archive requires a directory store")?;
        let placement: Option<(String, String, String)> = layout
            .catalog
            .lock()
            .unwrap()
            .query_row(
                "SELECT root_id,file,storage_state FROM sessions WHERE id=?1",
                [session],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let (root, file, state) = placement.context("Unknown session")?;
        if state == "archived" {
            return Ok(false);
        }
        let chat = self.open_chat_file(&root, &file, false)?;
        let sessions = chat.list_sessions()?;
        let ids: Vec<_> = sessions.iter().map(|session| session.id.clone()).collect();
        let runtime = self.runtime();
        let Ok(_active) = runtime.delete_guard(&ids) else {
            return Ok(false);
        };
        let pending=chat.read(|conn|Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM metadata WHERE substr(key,1,11)='quota_wait:') OR EXISTS(SELECT 1 FROM runs WHERE status IN ('running','waiting_for_quota')) OR EXISTS(SELECT 1 FROM inputs WHERE state IN ('pending','reminder_pending')) OR EXISTS(SELECT 1 FROM approvals WHERE state='pending') OR EXISTS(SELECT 1 FROM effects WHERE state='dispatched')",[],|row|row.get::<_,bool>(0))?))?;
        if pending {
            return Ok(false);
        }
        // The cache and this local handle are the only permitted references.
        if Arc::strong_count(&chat.0) != 2 {
            return Ok(false);
        }
        // Timestamp the archive transition rather than the last message age.
        // Existing metadata keeps this rebuildable without a schema migration.
        let archived_at = now();
        chat.write(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO metadata VALUES('archived_at',?1)",
                [&archived_at],
            )?;
            Ok(())
        })?;
        // Drain the metadata write's queued active projection before changing
        // placement, so a later flush cannot overwrite the archive transition.
        self.flush_serving()?;
        chat.read(|conn| {
            let busy: i64 =
                conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
            anyhow::ensure!(busy == 0, "Chat checkpoint is busy");
            Ok(())
        })?;
        let targets = chat.read(|conn| {
            ids.iter()
                .map(|session| {
                    Ok((
                        session.clone(),
                        conn.query_row(
                            "SELECT COALESCE(MAX(seq),0) FROM messages WHERE session_id=?1",
                            [session],
                            |row| row.get::<_, i64>(0),
                        )?,
                    ))
                })
                .collect::<Result<Vec<_>>>()
        })?;
        let path = chat.0.path.clone().context("Chat path missing")?;
        let mut chats = layout.chats.lock().unwrap();
        if Arc::strong_count(&chat.0) != 2 {
            return Ok(false);
        }
        chats.remove(&root);
        drop(chat);
        layout.forget_root(&root);
        // Hold cache placement while physical paths transition; no second opener.
        self.set_chat_state_targets(&root, "archiving", &targets, Some(&archived_at))?;
        cold::pack(&path)?;
        for suffix in ["-wal", "-shm"] {
            let sidecar = PathBuf::from(format!("{}{suffix}", path.display()));
            if sidecar.exists() {
                std::fs::remove_file(sidecar)?;
            }
        }
        std::fs::remove_file(&path)?;
        cold::sync_parent(&path)?;
        self.set_chat_state(&root, "archived")?;
        drop(chats);
        Ok(true)
    }
    /// Archived root IDs older than a strict RFC3339 cutoff. Older archives with
    /// no recorded transition time remain ineligible for automatic deletion.
    /// This reads metadata without restoring archived content.
    pub fn archived_roots_before(&self, cutoff: &str) -> Result<Vec<String>> {
        self.flush_serving()?;
        let layout = self
            .0
            .layout
            .as_ref()
            .context("Archive retention requires a directory store")?;
        let catalog = layout.catalog.lock().unwrap();
        let mut statement = catalog.prepare("SELECT s.id FROM sessions s JOIN metadata m ON m.key='archived_at:'||s.id WHERE s.id=s.root_id AND s.storage_state='archived' AND julianday(m.value)<julianday(?1) AND NOT EXISTS(SELECT 1 FROM sessions child WHERE child.root_id=s.id AND child.storage_state!='archived') ORDER BY s.id")?;
        Ok(statement
            .query_map([cutoff], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }
    /// Archive roots whose entire tree has been idle beyond the configured age.
    pub fn archive_idle_chats(&self) -> Result<usize> {
        let layout = self
            .0
            .layout
            .as_ref()
            .context("Archive requires a directory store")?;
        let roots: Vec<(String, String, Option<String>)> = {
            let catalog = layout.catalog.lock().unwrap();
            let mut statement=catalog.prepare("SELECT s.root_id,MAX(s.last_activity),m.value FROM sessions s LEFT JOIN metadata m ON m.key='root_storage:'||s.root_id GROUP BY s.root_id HAVING MIN(s.storage_state)='active' AND MAX(s.storage_state)='active'")?;
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut count = 0;
        let instant = chrono::Utc::now();
        for (root, activity, value) in roots {
            let policy = value
                .map(|value| serde_json::from_str::<Option<RootStoragePolicy>>(&value))
                .transpose()?
                .flatten();
            let days = policy.map_or(layout.options.archive_after_days, |policy| {
                policy.store.archive_after_days
            });
            if days == 0 {
                continue;
            }
            let cutoff = instant
                .checked_sub_signed(chrono::Duration::days(i64::from(days)))
                .context("Archive retention is outside the supported date range")?
                .to_rfc3339();
            if activity >= cutoff {
                continue;
            }
            if self.archive_chat(&root)? {
                count += 1;
            }
        }
        Ok(count)
    }
    fn recover_archives(&self) -> Result<()> {
        let layout = self.0.layout.as_ref().unwrap();
        let rows: Vec<(String, String, String)> = {
            let catalog = layout.catalog.lock().unwrap();
            let mut statement = catalog
                .prepare("SELECT root_id,file,storage_state FROM sessions GROUP BY root_id")?;
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        for (root, file, state) in rows {
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
            let path = layout.data_dir.join(relative);
            let archive = cold::archive_path(&path);
            if state == "archiving" && path.is_file() {
                let valid = archive.is_file() && cold::verify_archive(&path, &archive).is_ok();
                if valid {
                    layout.chats.lock().unwrap().remove(&root);
                    layout.forget_root(&root);
                    std::fs::remove_file(&path)?;
                    for suffix in ["-wal", "-shm"] {
                        let sidecar = PathBuf::from(format!("{}{suffix}", path.display()));
                        if sidecar.exists() {
                            std::fs::remove_file(sidecar)?;
                        }
                    }
                    cold::sync_parent(&path)?;
                    self.set_chat_state(&root, "archived")?;
                } else {
                    if archive.is_file() {
                        std::fs::remove_file(&archive)?;
                    }
                    self.set_chat_state(&root, "active")?;
                }
            } else if state == "archiving" && archive.is_file() {
                let conn = open_record(&path, true, true)?;
                let integrity: String =
                    conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
                anyhow::ensure!(
                    integrity == "ok",
                    "Interrupted archive failed integrity check"
                );
                self.set_chat_state(&root, "archived")?;
            } else if state == "archived" && path.is_file() {
                // Restore installed a verified original before updating catalog.
                if archive.is_file() {
                    cold::verify_archive(&path, &archive)?;
                    std::fs::remove_file(&archive)?;
                }
                self.set_chat_state(&root, "active")?;
            }
            anyhow::ensure!(
                state == "active" || path.is_file() || archive.is_file(),
                "Interrupted archive has neither original nor compressed record"
            );
            let part = cold::staged_path(&archive, "part");
            if part.exists() && (path.is_file() || archive.is_file()) {
                std::fs::remove_file(part)?;
            }
            let restore = cold::staged_path(&path, "restore-part");
            if restore.exists() && archive.is_file() {
                std::fs::remove_file(restore)?;
            }
        }
        Ok(())
    }
    pub(super) fn layout_checkpoint(&self) -> Result<()> {
        self.checkpoint_layout(true)
    }

    pub(super) fn layout_runtime_idle(&self) -> Result<()> {
        self.checkpoint_layout(false)
    }

    fn checkpoint_layout(&self, force: bool) -> Result<()> {
        self.housekeeping()?;
        self.flush_serving()?;
        let layout = self.0.layout.as_ref().unwrap();
        let chats: Vec<_> = layout.chats.lock().unwrap().values().cloned().collect();
        for chat in chats {
            if force || wal_needs_checkpoint(chat.0.path.as_deref()) {
                chat.idle_checkpoint()?;
            }
        }
        if force || wal_needs_checkpoint(self.0.path.as_deref()) {
            self.read(|conn| {
                conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
                Ok(())
            })?;
        }
        if force || wal_needs_checkpoint(Some(&layout.data_dir.join("catalog.db"))) {
            layout
                .catalog
                .lock()
                .unwrap()
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        }
        self.read(|conn| {
            let free: i64 = conn.query_row("PRAGMA freelist_count", [], |row| row.get(0))?;
            if free > 32 {
                conn.execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")?;
            }
            Ok(())
        })?;
        Ok(())
    }
    pub(super) fn layout_delete(&self, session_id: &str, preserve_effects: bool) -> Result<()> {
        self.flush_serving()?;
        let runtime = self.runtime();
        let last_event = runtime.log.last();
        let layout = self.0.layout.as_ref().unwrap();
        let chat = self.scoped(session_id)?.unwrap();
        let sessions = chat.list_sessions()?;
        let mut removed = vec![session_id.to_owned()];
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
            if before == removed.len() {
                break;
            }
        }
        let mut admission = runtime.delete_guard(&removed)?;
        for session in &removed {
            let running = chat.read(|conn| {
                Ok(conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM runs WHERE session_id=?1 AND status IN ('running','waiting_for_quota'))",
                    [session],
                    |row| row.get::<_, bool>(0),
                )?)
            })?;
            anyhow::ensure!(!running, "Cannot delete a session with a running run");
            if preserve_effects {
                let unresolved = chat.read(|conn| Ok(conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM effects WHERE session_id=?1 AND state!='completed')",
                    [session], |row| row.get::<_, bool>(0),
                )?))?;
                anyhow::ensure!(
                    !unresolved,
                    "Cannot delete a session with unresolved effects"
                );
            }
        }
        // Deleted children stay in an open chat; their placements must not.
        layout.forget_sessions(&removed);
        let path = chat.0.path.clone().unwrap();
        let root = path.file_stem().unwrap().to_string_lossy().into_owned();
        if session_id == root {
            layout.chats.lock().unwrap().remove(&root);
            layout.forget_root(&root);
            // Close every handle to the chat before deleting its SQLite files.
            release::remove_chat_files(chat, &path)?;
        } else {
            chat.write(|tx| {
                if preserve_effects {
                    for session in &removed {
                        let unresolved: bool = tx.query_row(
                            "SELECT EXISTS(SELECT 1 FROM effects WHERE session_id=?1 AND state!='completed')",
                            [session], |row| row.get(0),
                        )?;
                        anyhow::ensure!(!unresolved, "Cannot delete a session with unresolved effects");
                    }
                }
                let imported: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='imported_derived')",
                    [],
                    |row| row.get(0),
                )?;
                for session in &removed {
                    Self::delete_execution_mode_tx(tx, session)?;
                    Self::clear_quota_wait_tx(tx, session, None)?;
                    if imported {
                        tx.execute(
                            "DELETE FROM imported_derived WHERE session_id=?1",
                            [session],
                        )?;
                    }
                    tx.execute(
                        "DELETE FROM metadata WHERE key=?1",
                        [format!("import:{session}")],
                    )?;
                    tx.execute(
                        "DELETE FROM metadata WHERE key=?1",
                        [format!("cold_compressed_through:{session}")],
                    )?;
                    tx.execute(
                        "DELETE FROM metadata WHERE key=?1",
                        [format!("cold_compression_pass:{session}")],
                    )?;
                }
                for session in &removed {
                    chat.delete_session_checkpoints(tx, session)?;
                    tx.execute("UPDATE request_usage SET message_seq=NULL WHERE session_id=?1 AND message_seq IS NOT NULL", [session])?;
                }
                for table in [
                    "reminder_metadata",
                    "approvals",
                    "approval_rules",
                        "host_state",
                    "attachments",
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
                    for session in &removed {
                        tx.execute(&format!("DELETE FROM {table} WHERE {column}=?1"), [session])?;
                    }
                }
                chat.collect_orphan_blobs(tx)?;
                Ok(())
            })?;
            chat.read(|conn| {
                // incremental_vacuum emits rows while reclaiming pages. Exhaust
                // the statement instead of stopping after its first sqlite step.
                let mut statement = conn.prepare("PRAGMA incremental_vacuum")?;
                let mut rows = statement.query([])?;
                while rows.next()?.is_some() {}
                drop(rows);
                drop(statement);
                conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
                Ok(())
            })?;
        }
        self.write(|tx| {
            Self::remove_search_sessions(tx, &removed)?;
            for table in [
                "events",
                "context_views",
                "runs",
                "inputs",
                "approvals",
                "telemetry",
                "sessions",
            ] {
                let column = if table == "sessions" {
                    "id"
                } else {
                    "session_id"
                };
                for session in &removed {
                    tx.execute(&format!("DELETE FROM {table} WHERE {column}=?1"), [session])?;
                }
            }
            crate::event_log::remember_last(tx, last_event)?;
            Ok(())
        })?;
        self.catalog_rebuild_mode(false)?;
        for session in &removed {
            admission.remove(session);
            runtime.forget_history(session);
        }
        self.cleanup_deleted_sessions(&removed)?;
        Ok(())
    }
    /// The text of every field in this chat that can name a blob hash, with
    /// message bodies decoded. Hash references are substrings of this text.
    /// Number of reference-text scans, for tests that assert pruning never scans.
    #[cfg(test)]
    pub(crate) fn reference_scans() -> usize {
        REFERENCE_SCANS.with(std::cell::Cell::get)
    }
    pub(crate) fn reference_text(&self, conn: &Connection) -> Result<String> {
        #[cfg(test)]
        REFERENCE_SCANS.with(|count| count.set(count.get() + 1));
        let mut references = String::new();
        for (table, column) in [
            ("messages", "data"),
            ("attachments", "blob_hash"),
            ("inputs", "content_blob"),
            ("context_checkpoints", "data"),
            ("context_checkpoint_versions", "summary_hash"),
            ("approvals", "arguments_ref"),
            ("effects", "detail"),
            ("runs", "data"),
        ] {
            let mut stmt = conn.prepare(&format!(
                "SELECT {column} FROM {table} WHERE {column} IS NOT NULL"
            ))?;
            for value in stmt.query_map([], |row| row.get::<_, String>(0))? {
                let value = value?;
                if table == "messages" {
                    references.push_str(&Self::decode_message_body(conn, &value)?.to_string());
                } else {
                    references.push_str(&value);
                }
                // Separate values, so adjacent hashes never run together.
                references.push('\n');
            }
        }
        if let Some(host) = self.host() {
            let sessions = {
                let mut stmt = conn.prepare("SELECT id FROM sessions")?;
                stmt.query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            host.read(|projection| {
                for (table, column) in [
                    ("events", "data"),
                    ("approvals", "arguments_ref"),
                    ("runs", "data"),
                ] {
                    let mut stmt = projection.prepare(&format!(
                        "SELECT {column} FROM {table} WHERE session_id=?1 AND {column} IS NOT NULL"
                    ))?;
                    for session in &sessions {
                        for row in stmt.query_map([session], |row| row.get::<_, String>(0))? {
                            references.push_str(&row?);
                            references.push('\n');
                        }
                    }
                }
                Ok(())
            })?;
        }
        Ok(references)
    }
    pub(crate) fn collect_orphan_blobs(&self, conn: &Connection) -> Result<()> {
        // Conservative reachability: content references occur in JSON/text fields.
        // False positives retain a blob; no false negative may remove live content.
        let mut references = self.reference_text(conn)?;
        let mut stmt = conn.prepare("SELECT hash FROM blobs")?;
        let hashes = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // Content-part blobs can themselves reference attachment blobs. Traverse
        // those edges before reclaiming any hash; arbitrary binary data is safe.
        let mut live = std::collections::HashSet::new();
        loop {
            let mut changed = false;
            for hash in &hashes {
                if references.contains(hash) && live.insert(hash.clone()) {
                    references.push_str(&String::from_utf8_lossy(&Self::blob_get(conn, hash)?));
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        for hash in hashes {
            if !live.contains(&hash) {
                conn.execute("DELETE FROM blobs WHERE hash=?1", [hash])?;
            }
        }
        Ok(())
    }
    /// Delete each candidate checkpoint preimage that no checkpoint record names.
    /// The lookup uses the `checkpoint_records_preimage` index and is bounded to
    /// one hash per candidate. Content rows and `blobs` are never read or changed.
    pub(crate) fn drop_unreferenced_checkpoint_blobs(
        &self,
        conn: &Connection,
        candidates: std::collections::BTreeSet<String>,
    ) -> Result<()> {
        for hash in &candidates {
            let named: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM checkpoint_records WHERE preimage_hash=?1)",
                [hash],
                |row| row.get(0),
            )?;
            if !named {
                conn.execute("DELETE FROM checkpoint_blobs WHERE hash=?1", [hash])?;
            }
        }
        Ok(())
    }

    /// Remove a session's checkpoint records, then the preimages they alone named.
    pub(crate) fn delete_session_checkpoints(
        &self,
        conn: &Connection,
        session_id: &str,
    ) -> Result<()> {
        let candidates: std::collections::BTreeSet<String> = {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT preimage_hash FROM checkpoint_records WHERE session_id=?1 AND preimage_hash IS NOT NULL",
            )?;
            stmt.query_map([session_id], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        conn.execute(
            "DELETE FROM checkpoint_records WHERE session_id=?1",
            [session_id],
        )?;
        self.drop_unreferenced_checkpoint_blobs(conn, candidates)
    }
    pub(super) fn layout_stats(&self) -> Result<StorageStats> {
        let layout = self.0.layout.as_ref().unwrap();
        let mut stats = StorageStats {
            page_bytes: 0,
            database_bytes: 0,
            wal_bytes: 0,
            transaction_count: self.revision(),
            message_count: 0,
            pending_input_count: 0,
            chat_bytes: 0,
            serving_bytes: 0,
            catalog_bytes: 0,
            chat_transaction_count: self
                .revision()
                .saturating_sub(layout.serving_transactions.load(Ordering::Relaxed)),
            catalog_transaction_count: layout.catalog_transactions.load(Ordering::Relaxed),
            serving_transaction_count: layout.serving_transactions.load(Ordering::Relaxed),
        };
        for entry in std::fs::read_dir(layout.data_dir.join("chats"))? {
            let path = entry?.path();
            if path.to_string_lossy().ends_with(".db.zst") {
                let unpacked = path.with_extension("");
                if unpacked.exists() {
                    continue;
                }
                let bytes = std::fs::metadata(&path)?.len();
                stats.database_bytes += bytes;
                stats.chat_bytes += bytes;
                let record = open_record(&unpacked, true, true)?;
                let pages: i64 = record.query_row("PRAGMA page_count", [], |row| row.get(0))?;
                let page_size: i64 = record.query_row("PRAGMA page_size", [], |row| row.get(0))?;
                stats.page_bytes += (pages * page_size) as u64;
                stats.message_count +=
                    record.query_row("SELECT COUNT(*) FROM messages", [], |row| {
                        row.get::<_, i64>(0)
                    })? as u64;
                stats.pending_input_count += record.query_row(
                    "SELECT COUNT(*) FROM inputs WHERE state='pending'",
                    [],
                    |row| row.get::<_, i64>(0),
                )? as u64;
                continue;
            }
            if path.extension().is_none_or(|extension| extension != "db") {
                continue;
            }
            let root = path.file_stem().unwrap().to_string_lossy();
            let chat = self.open_chat(&root)?;
            let part = chat.storage_stats()?;
            stats.page_bytes += part.page_bytes;
            stats.database_bytes += part.database_bytes;
            stats.chat_bytes += part.database_bytes;
            stats.wal_bytes += part.wal_bytes;
            stats.message_count += part.message_count;
            stats.pending_input_count += part.pending_input_count;
        }
        for (name, target) in [
            ("serving.db", &mut stats.serving_bytes),
            ("catalog.db", &mut stats.catalog_bytes),
        ] {
            let path = layout.data_dir.join(name);
            *target = std::fs::metadata(&path)?.len();
            stats.database_bytes += *target;
            stats.wal_bytes += std::fs::metadata(format!("{}-wal", path.display()))
                .map(|metadata| metadata.len())
                .unwrap_or(0);
        }
        Ok(stats)
    }
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
mod ownership_tests {
    use super::*;

    #[test]
    fn live_clones_share_ownership_and_keep_other_owners_out() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let store = Store::open_data_dir(directory.path())?;
        let clone = store.clone();
        let reopened = Store::open_data_dir(directory.path())?;
        assert!(Arc::ptr_eq(&store.0, &reopened.0));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(directory.path().join("store.lock"))?;
        assert!(fs2::FileExt::try_lock_exclusive(&file).is_err());
        drop(store);
        drop(reopened);
        assert!(fs2::FileExt::try_lock_exclusive(&file).is_err());
        drop(clone);
        let _owner = FileLock::acquire(file, "last clone must release ownership")?;
        // A real independent owner still fails immediately; no acquire retry.
        assert!(Store::open_data_dir(directory.path()).is_err());
        Ok(())
    }

    #[test]
    fn reopening_waits_for_the_previous_owner_to_finish_closing() -> Result<()> {
        use std::{sync::mpsc, time::Duration};

        let directory = tempfile::tempdir()?;
        let store = Store::open_data_dir(directory.path())?;
        let session = store.create_session("buffered before close")?;
        let weak = Arc::downgrade(&store.0);
        // Hold the flush boundary so the last Arc expires while close is in progress.
        let connection = store.0.connection.clone();
        let guard = connection.lock().unwrap();
        let result = std::thread::scope(|scope| {
            let closing = scope.spawn(move || drop(store));
            let started = std::time::Instant::now();
            while weak.strong_count() != 0 {
                assert!(started.elapsed() < Duration::from_secs(5));
                std::thread::yield_now();
            }
            let (send, receive) = mpsc::channel();
            let path = directory.path();
            scope.spawn(move || assert!(send.send(Store::open_data_dir(path)).is_ok()));
            // Before the fix this returns WouldBlock while the old flush is blocked.
            let early = receive.recv_timeout(Duration::from_millis(100));
            drop(guard);
            closing.join().unwrap();
            early.unwrap_or_else(|_| receive.recv_timeout(Duration::from_secs(5)).unwrap())
        });
        drop(connection);
        let reopened = result?;
        assert_eq!(reopened.session(&session.id)?.title, session.title);
        Ok(())
    }

    #[cfg(unix)]
    struct BlockedFork {
        parent: std::os::unix::net::UnixStream,
        spawning: Option<std::thread::JoinHandle<std::io::Result<std::process::ExitStatus>>>,
    }
    #[cfg(unix)]
    impl BlockedFork {
        fn new() -> std::io::Result<Self> {
            use std::{
                io::{Read, Write},
                os::unix::{net::UnixStream, process::CommandExt},
                process::Command,
                time::Duration,
            };

            let (parent, mut child) = UnixStream::pair()?;
            parent.set_read_timeout(Some(Duration::from_secs(5)))?;
            child.set_read_timeout(Some(Duration::from_secs(5)))?;
            let mut command = Command::new("true");
            // SAFETY: only reads/writes an existing socket before exec; no
            // allocation, locking or other non-async-signal-safe work occurs.
            unsafe {
                command.pre_exec(move || {
                    child.write_all(b"r")?;
                    child.read_exact(&mut [0])?;
                    Ok(())
                });
            }
            let mut fork = Self {
                parent,
                spawning: Some(std::thread::spawn(move || command.status())),
            };
            fork.parent.read_exact(&mut [0])?;
            Ok(fork)
        }
        fn finish(&mut self) -> std::io::Result<()> {
            use std::io::Write;

            if let Some(spawning) = self.spawning.take() {
                let released = self.parent.write_all(b"c");
                let status = spawning.join().expect("fork thread panicked")?;
                released?;
                assert!(status.success());
            }
            Ok(())
        }
    }
    #[cfg(unix)]
    impl Drop for BlockedFork {
        fn drop(&mut self) {
            // Release and reap our child even when the regression fails.
            let _ = self.finish();
        }
    }

    #[cfg(unix)]
    #[test]
    fn closing_unlocks_before_an_unrelated_fork_execs() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let store = Store::open_data_dir(directory.path())?;
        let session = store.create_session("inherited lock")?;
        let mut fork = BlockedFork::new()?;
        drop(store);
        let reopened = Store::open_data_dir(directory.path());
        fork.finish()?;
        assert_eq!(reopened?.session(&session.id)?.title, session.title);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn export_releases_its_shared_lock_before_an_unrelated_fork_execs() -> Result<()> {
        use std::io::{self, Write};

        struct ForkOnWrite<'a>(&'a mut Option<BlockedFork>);
        impl Write for ForkOnWrite<'_> {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.0.is_none() {
                    *self.0 = Some(BlockedFork::new()?);
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let directory = tempfile::tempdir()?;
        let store = Store::open_data_dir(directory.path())?;
        let session = store.create_session("export shared lock")?;
        store.append_batch(&[Store::message(&session.id, Role::User, "original".into())])?;
        drop(store);
        let mut fork = None;
        assert_eq!(
            Store::export_data_dir_jsonl(directory.path(), &session.id, ForkOnWrite(&mut fork))?,
            1
        );
        let reopened = Store::open_data_dir(directory.path());
        fork.as_mut().unwrap().finish()?;
        assert_eq!(reopened?.messages(&session.id)?[0].content, "original");
        Ok(())
    }

    fn legacy(path: &Path) -> Result<()> {
        let conn = Connection::open(path)?;
        conn.execute_batch(include_str!("../tests/fixtures/main-v6-schema.sql"))?;
        let session = Session {
            id: id(),
            title: "ownership fixture".into(),
            parent_session_id: None,
            parent_cutoff: None,
            created_at: now(),
            updated_at: now(),
            read_only: false,
            hidden: false,
            context_strategy: ContextStrategy::default(),
        };
        conn.execute(
            "INSERT INTO sessions VALUES(?1,?2)",
            params![session.id, serde_json::to_string(&session)?],
        )?;
        let message = Store::message(&session.id, Role::User, "synthetic accepted input".into());
        conn.execute(
            "INSERT INTO messages(session_id,data) VALUES(?1,?2)",
            params![session.id, serde_json::to_string(&message)?],
        )?;
        Ok(())
    }
    fn owner(path: &Path) -> Result<FileLock> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        FileLock::acquire(file, "test owner lock")
    }
    #[test]
    fn migration_refuses_a_busy_legacy_source_and_retries_after_close() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("archive.sqlite");
        let target = directory.path().join("data");
        legacy(&source)?;
        let original = std::fs::read(&source)?;
        let lock = owner(&source.with_extension("sqlite.lock"))?;
        assert!(
            Store::migrate_legacy(&source, &target)
                .unwrap_err()
                .to_string()
                .contains("Legacy database is already owned")
        );
        assert!(!target.exists());
        assert_eq!(std::fs::read(&source)?, original);
        drop(lock);
        Store::migrate_legacy(&source, &target)?;
        assert_eq!(Store::open_data_dir(&target)?.list_sessions()?.len(), 1);
        assert_eq!(std::fs::read(&source)?, original);
        Ok(())
    }
    #[test]
    fn migration_refuses_a_destination_owned_before_catalog_creation() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("archive.sqlite");
        let target = directory.path().join("data");
        legacy(&source)?;
        std::fs::create_dir(&target)?;
        let lock_path = target.join("store.lock");
        let lock = owner(&lock_path)?;
        assert!(
            Store::migrate_legacy(&source, &target)
                .unwrap_err()
                .to_string()
                .contains("destination is already owned")
        );
        let another = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)?;
        assert!(
            fs2::FileExt::try_lock_exclusive(&another).is_err(),
            "installer must not replace the held lock inode"
        );
        assert!(!target.join("catalog.db").exists());
        drop(lock);
        Ok(())
    }
    #[test]
    fn installer_rechecks_a_concurrently_created_catalog_before_moving_files() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("first.sqlite");
        let target = directory.path().join("data");
        legacy(&source)?;
        Store::migrate_legacy(&source, &target)?;
        let original = std::fs::read(target.join("serving.db"))?;
        let staged = directory.path().join("second-stage");
        std::fs::create_dir_all(staged.join("chats"))?;
        std::fs::write(staged.join("serving.db"), b"must not be installed")?;
        let error = Store::install_migration(&staged, &target, "another-source").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("another source or by another host")
        );
        assert_eq!(std::fs::read(target.join("serving.db"))?, original);
        assert_eq!(
            std::fs::read(staged.join("serving.db"))?,
            b"must not be installed"
        );
        Ok(())
    }
    #[test]
    fn failed_installation_removes_its_new_lock_and_allows_a_retry() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("archive.sqlite");
        let target = directory.path().join("data");
        legacy(&source)?;
        let staged = directory.path().join("incomplete-stage");
        std::fs::create_dir_all(staged.join("chats"))?;
        assert!(Store::install_migration(&staged, &target, "source").is_err());
        assert!(
            !target.join("store.lock").exists(),
            "failed bootstrap must not look like a completed layout"
        );
        Store::migrate_legacy(&source, &target)?;
        assert_eq!(Store::open_data_dir(&target)?.list_sessions()?.len(), 1);
        Ok(())
    }
    struct NoModelCalls;
    #[async_trait::async_trait]
    impl crate::Provider for NoModelCalls {
        async fn generate(
            &self,
            _: crate::ModelRequest,
            _: crate::TextCallback,
            _: crate::CancellationToken,
        ) -> Result<crate::ModelResponse> {
            bail!("This storage test must not call a model")
        }
    }
    #[test]
    fn rebuild_reconciles_an_interrupted_root_delete_and_invalidates_replay() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let (doomed, kept, doomed_path, kept_path) = {
            let store = Store::open_data_dir(directory.path())?;
            let doomed = store.create_session("doomed")?;
            let kept = store.create_session("kept")?;
            store.runtime().log.emit(
                &store,
                &doomed.id,
                "doomed-run",
                crate::EventKind::FollowupQueued {
                    text: "synthetic deleted content".into(),
                },
                None,
            );
            store.runtime().log.emit(
                &store,
                &kept.id,
                "kept-run",
                crate::EventKind::FollowupQueued {
                    text: "synthetic retained content".into(),
                },
                None,
            );
            let doomed_path = store.chat_path(&doomed.id)?.unwrap();
            let kept_path = store.chat_path(&kept.id)?.unwrap();
            store.idle_checkpoint()?;
            (doomed, kept, doomed_path, kept_path)
        };
        let kept_bytes = std::fs::read(&kept_path)?;
        // Simulate interruption after the authoritative root file was removed,
        // before disposable serving rows/catalog were cleaned.
        std::fs::remove_file(doomed_path)?;
        let store = Store::open_data_dir(directory.path())?;
        let harness = crate::Harness::new(
            store.clone(),
            Arc::new(NoModelCalls),
            crate::ToolRegistry::new(),
            crate::HarnessConfig::default(),
        );
        let events = harness.events_after(0, None, 1000)?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].session_id, kept.id);
        assert_eq!(events[0].sequence, 2);
        assert!(!harness.can_resume_from(0));
        assert!(!harness.can_resume_from(1));
        assert!(harness.can_resume_from(harness.last_sequence()));
        assert!(store.session(&doomed.id).is_err());
        assert_eq!(std::fs::read(&kept_path)?, kept_bytes);
        Ok(())
    }
}

#[cfg(test)]
#[path = "store_layout_checkpoint_tests.rs"]
mod checkpoint_prune_tests;
