//! One-time import of legacy per-workspace stores into the central store.
//!
//! Sources are only read: under a shared lease on their `store.lock`, their
//! record files are byte-copied into a private staging directory inside the
//! central store. Everything else works on those copies. A source whose lease
//! is held by a running process is skipped and retried on the next start.
use super::*;
use rusqlite::OpenFlags;
use std::collections::BTreeMap;

// Previous default stores and the per-workspace stores beside them.
#[path = "store_legacy_sources.rs"]
mod sources;

/// Rows per write transaction when merging shared data-directory facts.
pub const LEGACY_IMPORT_BATCH: usize = 256;
const MARKER_PREFIX: &str = "legacy_import:";
const PROVENANCE: &str = "legacy_import_source";
const STAGING: &str = "legacy-import";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyImportStatus {
    /// Imported in this call and recorded in the central catalog.
    Imported,
    /// Recorded by an earlier import; nothing was read.
    AlreadyImported,
    /// Another process owns the source; retried on the next start.
    Locked,
    /// Not imported and not recorded; retried on the next start.
    Skipped(String),
    /// Not importable as it stands (an unmigrated `archive.sqlite`). Recorded
    /// so the notice appears once; retried only after the source is migrated.
    Unsupported(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct LegacyImportReport {
    pub source: PathBuf,
    pub status: LegacyImportStatus,
    /// Root chat files installed by this call (resumed ones included).
    pub chats: usize,
    /// Quota history rows from this source added to the central catalog.
    pub quota_rows: usize,
    /// Root chats whose id already existed centrally with other content:
    /// imported under a new root id (old id to new id).
    pub renamed: BTreeMap<String, String>,
    /// Root chats already present centrally with the same content; not copied.
    pub duplicates: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct LegacyImportSummary {
    pub sources: Vec<LegacyImportReport>,
    /// Write transactions committed by the import itself: one per staged chat
    /// copy, one per quota batch and one for the import receipts. Host-side
    /// merges report their own transactions.
    pub transactions: u64,
}

/// A read-only snapshot of one legacy store, staged inside the central store.
/// Top-level databases other than the disposable serving and web caches are
/// copied with any non-empty WAL; `chats/` holds the root chat files.
pub struct LegacySnapshot {
    pub source: PathBuf,
    pub dir: PathBuf,
}

enum Snapshot {
    Ready(LegacySnapshot),
    Locked,
    Skipped(String),
    Unsupported(String),
}

#[derive(Default)]
struct Installed {
    chats: usize,
    renamed: BTreeMap<String, String>,
    duplicates: Vec<String>,
}

/// One quota history row during a merge: an existing central row (by id) or a
/// reading from a source (by report index).
struct QuotaRow {
    time: chrono::DateTime<chrono::FixedOffset>,
    origin: QuotaOrigin,
    reading: crate::QuotaReading,
    /// The JSON without the balance, as `quota_history.data` stores it.
    data: String,
    /// The account balance stored beside the change, if any.
    balance: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum QuotaOrigin {
    Central(i64),
    Source(usize),
}

/// A 64-character hexadecimal directory name: the old per-workspace default.
pub fn is_legacy_store_name(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn copy_with_wal(source: &Path, target: &Path) -> Result<()> {
    std::fs::copy(source, target)?;
    let wal = PathBuf::from(format!("{}-wal", source.display()));
    if std::fs::metadata(&wal).is_ok_and(|metadata| metadata.len() > 0) {
        // Committed pages of an unclean shutdown. SQLite replays them when the
        // copy is opened; the shared-memory index is rebuilt, never copied.
        std::fs::copy(&wal, format!("{}-wal", target.display()))?;
    }
    Ok(())
}

fn take_snapshot(source: &Path, target: &Path) -> Result<Snapshot> {
    if !source.join("catalog.db").is_file() {
        if source.join("archive.sqlite").is_file() {
            return Ok(Snapshot::Unsupported(
                "unmigrated legacy database; open it once with --data-dir to migrate it,                  and it is imported on the next start"
                    .into(),
            ));
        }
        return Ok(Snapshot::Skipped("not a BubblePaw directory store".into()));
    }
    // Opening the lease file read-only never changes its bytes. A live owner
    // holds it exclusively, so a shared lease proves no writer exists.
    let Ok(lock) = std::fs::File::open(source.join("store.lock")) else {
        return Ok(Snapshot::Skipped("store.lock is missing".into()));
    };
    let Ok(_lease) = FileLock::acquire_shared(lock) else {
        return Ok(Snapshot::Locked);
    };
    std::fs::create_dir_all(target.join("chats"))?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_file()
            && name.ends_with(".db")
            && !["serving.db", "web-cache.db"].contains(&name.as_str())
        {
            copy_with_wal(&entry.path(), &target.join(&name))?;
        }
    }
    let chats = source.join("chats");
    if chats.is_dir() {
        for entry in std::fs::read_dir(&chats)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".db") {
                copy_with_wal(&entry.path(), &target.join("chats").join(&name))?;
            } else if let Some(stem) = name.strip_suffix(".db.zst")
                // An active file wins over a leftover archive, as in rebuild.
                && !chats.join(format!("{stem}.db")).is_file()
            {
                std::fs::copy(entry.path(), target.join("chats").join(&name))?;
            }
        }
    }
    for kind in SESSION_FILE_DIRS {
        copy_session_files(&source.join(kind), &target.join(kind))?;
    }
    Ok(Snapshot::Ready(LegacySnapshot {
        source: source.to_owned(),
        dir: target.to_owned(),
    }))
}

/// Per-session file directories copied with each chat: spill output and
/// artifacts, both keyed by session id.
const SESSION_FILE_DIRS: [&str; 2] = ["spill", "artifacts"];

/// Copy `<source>/<session>/<file>` for every session directory, regular
/// files only. Symlinks and spill locks are not data and are left out.
fn copy_session_files(source: &Path, target: &Path) -> Result<()> {
    if !std::fs::symlink_metadata(source).is_ok_and(|metadata| metadata.is_dir()) {
        return Ok(());
    }
    for session in std::fs::read_dir(source)? {
        let session = session?;
        if !session.file_type()?.is_dir() {
            continue;
        }
        let destination = target.join(session.file_name());
        for file in std::fs::read_dir(session.path())? {
            let file = file?;
            if !file.file_type()?.is_file() || file.file_name() == ".lock" {
                continue;
            }
            std::fs::create_dir_all(&destination)?;
            std::fs::copy(file.path(), destination.join(file.file_name()))?;
        }
    }
    Ok(())
}

/// The forms a path takes in stored text: raw, and JSON-escaped once or
/// twice (a tool receipt is JSON inside a JSON message).
fn stored_forms(path: &Path) -> Result<Vec<String>> {
    let mut forms = vec![path.to_string_lossy().into_owned()];
    for _ in 0..2 {
        let escaped = serde_json::to_string(forms.last().unwrap())?;
        forms.push(escaped[1..escaped.len() - 1].to_owned());
    }
    Ok(forms)
}

fn metadata_value(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM metadata WHERE key=?1", [key], |row| {
            row.get(0)
        })
        .optional()?)
}

fn quota_readings(catalog: &Path) -> Result<Vec<crate::QuotaReading>> {
    if !catalog.is_file() {
        return Ok(Vec::new());
    }
    let conn = Connection::open(catalog)?;
    let application: i64 = conn.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    anyhow::ensure!(application == 0x42504231, "Unsupported legacy catalog");
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='quota_history')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(Vec::new());
    }
    // A v1 catalog has no balance column; its rows carry none.
    let has_balance: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('quota_history') WHERE name='balance')",
        [],
        |row| row.get(0),
    )?;
    let balance = if has_balance { "balance" } else { "NULL" };
    let mut statement = conn.prepare(&format!(
        "SELECT data,{balance} FROM quota_history ORDER BY id"
    ))?;
    statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .map(|row| {
            let (data, balance) = row?;
            let mut reading: crate::QuotaReading = serde_json::from_str(&data)?;
            reading.validate()?;
            reading.balance = balance;
            Ok(reading)
        })
        .collect()
}

fn observed(reading: &crate::QuotaReading) -> Result<chrono::DateTime<chrono::FixedOffset>> {
    Ok(chrono::DateTime::parse_from_rfc3339(&reading.observed_at)?)
}

/// The change-only rule: same used percentage and the same reset instant.
fn same_quota(left: &crate::QuotaReading, right: &crate::QuotaReading) -> Result<bool> {
    let reset = |reading: &crate::QuotaReading| {
        reading
            .resets_at
            .as_deref()
            .map(chrono::DateTime::parse_from_rfc3339)
            .transpose()
    };
    Ok(left.used_percent == right.used_percent && reset(left)? == reset(right)?)
}

/// What decides whether two chat files hold the same chat: their sessions,
/// their last durable message sequence and that message's exact bytes.
fn chat_fingerprint(conn: &Connection) -> Result<(i64, i64, Option<i64>, Option<String>)> {
    let sessions = conn.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
    let (messages, last): (i64, Option<i64>) =
        conn.query_row("SELECT COUNT(*),MAX(seq) FROM messages", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
    let hash = conn
        .query_row(
            "SELECT session_id,data FROM messages WHERE seq=?1",
            [last],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
        .map(|(session, data)| {
            let mut hasher = Sha256::new();
            hasher.update(session.as_bytes());
            hasher.update([0u8]);
            hasher.update(data.as_bytes());
            format!("{:x}", hasher.finalize())
        });
    Ok((sessions, messages, last, hash))
}

/// A stable new session id for `id` from `source`, so a resumed import picks
/// the same ids and recognizes chats it already installed.
/// The request ids of a chat's usage rows; none for a chat without the table.
fn request_ids(conn: &Connection) -> Result<Vec<String>> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='request_usage')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(Vec::new());
    }
    Ok(conn
        .prepare("SELECT request_id FROM request_usage")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

fn remapped_id(source: &str, id: &str) -> String {
    let digest = Sha256::digest(format!("legacy-import:{source}:{id}").as_bytes());
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest[..16]);
    uuid::Builder::from_random_bytes(bytes)
        .into_uuid()
        .to_string()
}

/// Replace `old` with `new` except where it is a path component: recorded
/// spill, artifact and job paths keep naming the unchanged legacy files.
fn replace_id(text: &str, old: &str, new: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut changed = false;
    for (position, _) in text.match_indices(old) {
        out.push_str(&text[last..position]);
        if matches!(text[..position].chars().next_back(), Some('/' | '\\')) {
            out.push_str(old);
        } else {
            out.push_str(new);
            changed = true;
        }
        last = position + old.len();
    }
    out.push_str(&text[last..]);
    changed.then_some(out)
}

/// Rewrite every session id of a staged chat file in every text cell.
fn remap_sessions(tx: &rusqlite::Transaction<'_>, map: &[(String, String)]) -> Result<()> {
    // Parent and child rows are renamed one at a time; check the foreign
    // keys once, at commit.
    tx.execute_batch("PRAGMA defer_foreign_keys=ON")?;
    for (old, new) in map {
        rewrite_text(tx, old, |text| replace_id(text, old, new))?;
    }
    Ok(())
}

/// Point stored spill paths of each session at its copy in the central
/// store: `<legacy>/spill/<old>` becomes `<central>/spill/<new>`.
fn rewrite_spill_paths(
    tx: &rusqlite::Transaction<'_>,
    legacy: &Path,
    central: &Path,
    map: &[(String, String)],
) -> Result<()> {
    for (old, new) in map {
        let from = stored_forms(&legacy.join("spill").join(old))?;
        let to = stored_forms(&central.join("spill").join(new))?;
        // Most escaped first, so a shorter form never splits a longer one.
        for (from, to) in from.iter().zip(&to).rev() {
            rewrite_text(tx, from, |text| Some(text.replace(from.as_str(), to)))?;
        }
    }
    Ok(())
}

/// Rewrite every text value containing `needle` in every table of a chat.
fn rewrite_text(
    tx: &rusqlite::Transaction<'_>,
    needle: &str,
    rewrite: impl Fn(&str) -> Option<String>,
) -> Result<()> {
    let tables: Vec<(String, String)> = tx
        .prepare(
            "SELECT name,sql FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
        )?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (table, sql) in tables {
        anyhow::ensure!(
            !sql.to_ascii_uppercase().contains("WITHOUT ROWID"),
            "Cannot renumber legacy chat table {table}"
        );
        let columns: Vec<String> = tx
            .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for column in columns {
            {
                let rows: Vec<(i64, String)> = tx
                    .prepare(&format!(
                        "SELECT rowid,\"{column}\" FROM \"{table}\"                          WHERE typeof(\"{column}\")='text' AND instr(\"{column}\",?1)>0"
                    ))?
                    .query_map([needle], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?;
                for (rowid, text) in rows {
                    if let Some(text) = rewrite(&text).filter(|changed| *changed != text) {
                        tx.execute(
                            &format!("UPDATE \"{table}\" SET \"{column}\"=?1 WHERE rowid=?2"),
                            params![text, rowid],
                        )?;
                    }
                }
            }
        }
    }
    Ok(())
}

impl Store {
    /// Import every legacy per-workspace store beside this central store.
    ///
    /// Each source is imported once and recorded in the central catalog. The
    /// import is idempotent: an interrupted run leaves no receipt, and the
    /// next run skips chat files it already installed and adds no quota row
    /// twice. `merge` receives each snapshot after its chats and quota were
    /// merged, for host-owned shared state such as the delegation ledger; it
    /// must be idempotent too. A failing source is reported, not recorded.
    pub fn import_legacy_stores(
        &self,
        merge: impl FnMut(&LegacySnapshot) -> Result<()>,
    ) -> Result<LegacyImportSummary> {
        self.import_legacy_stores_with(&[], merge)
    }

    /// Import one batch of candidates: see `import_legacy_stores_with`.
    fn import_candidates(
        &self,
        candidates: Vec<(String, PathBuf, bool)>,
        mut merge: impl FnMut(&LegacySnapshot) -> Result<()>,
    ) -> Result<LegacyImportSummary> {
        let layout = self.0.layout.as_ref().unwrap();
        let mut summary = LegacyImportSummary::default();
        let staging = layout.data_dir.join(STAGING);
        if staging.exists() {
            // Only this import writes here; a leftover is an interrupted run.
            std::fs::remove_dir_all(&staging)?;
        }
        let mut snapshots = Vec::new();
        let mut unsupported = Vec::new();
        for (name, source, pending) in candidates {
            let mut report = LegacyImportReport {
                source: source.clone(),
                status: LegacyImportStatus::AlreadyImported,
                chats: 0,
                quota_rows: 0,
                renamed: BTreeMap::new(),
                duplicates: Vec::new(),
            };
            if !pending {
                summary.sources.push(report);
                continue;
            }
            match take_snapshot(&source, &staging.join(&name)) {
                Ok(Snapshot::Ready(snapshot)) => {
                    report.status = LegacyImportStatus::Imported;
                    snapshots.push((name, snapshot, summary.sources.len()));
                }
                Ok(Snapshot::Locked) => report.status = LegacyImportStatus::Locked,
                Ok(Snapshot::Skipped(reason)) => {
                    report.status = LegacyImportStatus::Skipped(reason)
                }
                Ok(Snapshot::Unsupported(reason)) => {
                    unsupported.push((name, summary.sources.len()));
                    report.status = LegacyImportStatus::Unsupported(reason);
                }
                Err(error) => {
                    report.status = LegacyImportStatus::Skipped(format!("{error:#}"));
                }
            }
            summary.sources.push(report);
        }
        let mut failed = std::collections::HashSet::new();
        let mut readings = Vec::new();
        for (_, snapshot, index) in &snapshots {
            let result = self.install_legacy_chats(snapshot, &mut summary.transactions);
            let quota = result.and_then(|installed| {
                let report = &mut summary.sources[*index];
                report.chats = installed.chats;
                report.renamed = installed.renamed;
                report.duplicates = installed.duplicates;
                quota_readings(&snapshot.dir.join("catalog.db"))
            });
            match quota {
                Ok(rows) => readings.extend(rows.into_iter().map(|row| (*index, row))),
                Err(error) => {
                    failed.insert(*index);
                    summary.sources[*index].status =
                        LegacyImportStatus::Skipped(format!("{error:#}"));
                }
            }
        }
        self.merge_legacy_quota(readings, &mut summary)?;
        for (_, snapshot, index) in &snapshots {
            if failed.contains(index) {
                continue;
            }
            if let Err(error) = merge(snapshot) {
                failed.insert(*index);
                summary.sources[*index].status = LegacyImportStatus::Skipped(format!("{error:#}"));
            }
        }
        if !snapshots.is_empty() {
            self.catalog_rebuild()?;
        }
        let mut receipts = Vec::new();
        for (name, snapshot, index) in &snapshots {
            if failed.contains(index) {
                continue;
            }
            let report = &summary.sources[*index];
            receipts.push((
                name.clone(),
                serde_json::json!({
                    "source": snapshot.source.to_string_lossy(),
                    "imported_at": now(),
                    "chats": report.chats,
                    "quota_rows": report.quota_rows,
                    "renamed": report.renamed,
                    "duplicates": report.duplicates,
                }),
            ));
        }
        for (name, index) in unsupported {
            let report = &summary.sources[index];
            if let LegacyImportStatus::Unsupported(reason) = &report.status {
                receipts.push((
                    name,
                    serde_json::json!({
                        "source": report.source.to_string_lossy(),
                        "recorded_at": now(),
                        "unsupported": reason,
                    }),
                ));
            }
        }
        if !receipts.is_empty() {
            let mut catalog = layout.catalog.lock().unwrap();
            let tx = catalog.transaction()?;
            for (name, receipt) in receipts {
                tx.execute(
                    "INSERT OR REPLACE INTO metadata VALUES(?1,?2)",
                    params![
                        format!("{MARKER_PREFIX}{name}"),
                        serde_json::to_string(&receipt)?
                    ],
                )?;
            }
            tx.commit()?;
            layout.catalog_transactions.fetch_add(1, Ordering::Relaxed);
            summary.transactions += 1;
        }
        drop(snapshots);
        if staging.exists() {
            std::fs::remove_dir_all(&staging)?;
        }
        Ok(summary)
    }

    /// Install every staged root chat that is not already installed. A chat
    /// without its own workspace binding receives the binding it had in its
    /// source store, so it keeps that workspace in the central store. A root
    /// id that already exists centrally is a duplicate when both files hold
    /// the same chat; otherwise the staged chat gets new session ids.
    fn install_legacy_chats(
        &self,
        snapshot: &LegacySnapshot,
        transactions: &mut u64,
    ) -> Result<Installed> {
        let layout = self.0.layout.as_ref().unwrap();
        let source = snapshot.source.to_string_lossy().into_owned();
        let store_workspace = {
            let catalog = snapshot.dir.join("catalog.db");
            let conn = Connection::open_with_flags(&catalog, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
            metadata_value(&conn, "workspace")?
        };
        // Spill paths are stored as the canonical data directory joins them.
        // A copied legacy store still points into the store it was copied
        // from, so paths into any legacy sibling are rewritten.
        let mut legacy_roots = vec![snapshot.source.canonicalize()?];
        if let Some(parent) = snapshot.source.parent() {
            for entry in std::fs::read_dir(parent)? {
                let entry = entry?;
                if is_legacy_store_name(&entry.file_name().to_string_lossy())
                    && entry.file_type()?.is_dir()
                    && let Ok(root) = entry.path().canonicalize()
                    && !legacy_roots.contains(&root)
                {
                    legacy_roots.push(root);
                }
            }
        }
        let central_root = layout.data_dir.canonicalize()?;
        let chats = snapshot.dir.join("chats");
        let mut roots = std::collections::BTreeSet::new();
        for entry in std::fs::read_dir(&chats)? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if let Some(root) = name
                .strip_suffix(".db.zst")
                .or_else(|| name.strip_suffix(".db"))
            {
                roots.insert(root.to_owned());
            }
        }
        let installed_at = |root: &str| {
            let path = layout.data_dir.join("chats").join(format!("{root}.db"));
            (path.is_file() || cold::archive_path(&path).is_file()).then_some(path)
        };
        let mut installed = Installed::default();
        for root in roots {
            anyhow::ensure!(
                !root.is_empty()
                    && root
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "Invalid legacy chat file name: {root}"
            );
            let staged = chats.join(format!("{root}.db"));
            let archive = cold::archive_path(&staged);
            if !staged.is_file() {
                // Archived chats are installed active; idle housekeeping
                // archives them again under the central policy.
                cold::unpack(&archive, &staged)?;
            }
            let mut conn = Connection::open(&staged)?;
            let application: i64 = conn.query_row("PRAGMA application_id", [], |row| row.get(0))?;
            anyhow::ensure!(
                application == 0x42504230
                    && metadata_value(&conn, "storage_layout")?.as_deref() == Some("chat-v1"),
                "Unsupported legacy chat record: {root}"
            );
            let ids: Vec<String> = conn
                .prepare("SELECT id FROM sessions ORDER BY id")?
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            anyhow::ensure!(ids.contains(&root), "Legacy chat {root} lacks its root");
            let mut target = root.clone();
            let mut remap = Vec::new();
            let mut sessions: Vec<(String, String)> =
                ids.iter().map(|id| (id.clone(), id.clone())).collect();
            let mut shared_usage = Vec::new();
            if let Some(existing) = installed_at(&root) {
                // Archived destinations are read through a temporary copy.
                let record = layout::open_record(&existing, true, false)
                    .with_context(|| format!("Read installed chat {root}"))?;
                if metadata_value(&record, PROVENANCE)?.as_deref() == Some(source.as_str()) {
                    // An interrupted import already installed this whole
                    // chat; any input accepted since then stays.
                    self.install_session_files(snapshot, &sessions)?;
                    installed.chats += 1;
                    continue;
                }
                if chat_fingerprint(&record)? == chat_fingerprint(&conn)? {
                    installed.duplicates.push(root);
                    continue;
                }
                // Usage of the history both copies share is already counted
                // by the original; the copy keeps only its own requests.
                shared_usage = request_ids(&record)?;
                drop(record);
                remap = ids
                    .iter()
                    .map(|id| (id.clone(), remapped_id(&source, id)))
                    .collect();
                sessions.clone_from(&remap);
                target = remapped_id(&source, &root);
                installed.renamed.insert(root.clone(), target.clone());
                if let Some(existing) = installed_at(&target) {
                    let record = layout::open_record(&existing, true, false)?;
                    anyhow::ensure!(
                        metadata_value(&record, PROVENANCE)?.as_deref() == Some(source.as_str()),
                        "Chat {target} already exists in the central store from another source"
                    );
                    self.install_session_files(snapshot, &sessions)?;
                    installed.chats += 1;
                    continue;
                }
            }
            let key = format!("root_workspace:{target}");
            self.install_session_files(snapshot, &sessions)?;
            let tx = conn.transaction()?;
            for request in &shared_usage {
                tx.execute("DELETE FROM request_usage WHERE request_id=?1", [request])?;
            }
            for legacy_root in &legacy_roots {
                rewrite_spill_paths(&tx, legacy_root, &central_root, &sessions)?;
            }
            if !remap.is_empty() {
                remap_sessions(&tx, &remap)?;
            }
            let binding = match metadata_value(&tx, &key)? {
                Some(_) => None,
                None => metadata_value(&tx, "workspace")?.or(store_workspace.clone()),
            };
            if let Some(binding) = &binding {
                Self::workspace_binding(binding)?;
                tx.execute("INSERT INTO metadata VALUES(?1,?2)", params![key, binding])?;
            }
            tx.execute(
                "INSERT OR REPLACE INTO metadata VALUES(?1,?2)",
                params![PROVENANCE, source],
            )?;
            tx.commit()?;
            *transactions += 1;
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
            drop(conn);
            for suffix in ["-wal", "-shm"] {
                let sidecar = PathBuf::from(format!("{}{suffix}", staged.display()));
                if sidecar.exists() {
                    std::fs::remove_file(sidecar)?;
                }
            }
            let destination = layout.data_dir.join("chats").join(format!("{target}.db"));
            // Windows flushes a file only through a handle with write access
            // (FlushFileBuffers); a read-only handle fails with os error 5.
            std::fs::OpenOptions::new()
                .write(true)
                .open(&staged)?
                .sync_all()?;
            std::fs::rename(&staged, &destination)?;
            cold::sync_parent(&destination)?;
            installed.chats += 1;
        }
        Ok(installed)
    }

    /// Move the staged spill and artifact files of each `(source, target)`
    /// session into the central store under the target id. A file already
    /// there was moved by an interrupted import and is kept.
    fn install_session_files(
        &self,
        snapshot: &LegacySnapshot,
        sessions: &[(String, String)],
    ) -> Result<()> {
        let layout = self.0.layout.as_ref().unwrap();
        for kind in SESSION_FILE_DIRS {
            for (old, new) in sessions {
                let staged = snapshot.dir.join(kind).join(old);
                if !staged.is_dir() {
                    continue;
                }
                let destination = layout.data_dir.join(kind).join(new);
                std::fs::create_dir_all(&destination)?;
                for file in std::fs::read_dir(&staged)? {
                    let file = file?;
                    let target = destination.join(file.file_name());
                    if !target.exists() {
                        std::fs::rename(file.path(), &target)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Merge source quota readings into the central change-only history in
    /// time order. Only exact duplicates and a reading equal to the previous
    /// change of its account and window are dropped. When the merged history
    /// is the central history followed by new rows, those rows are appended;
    /// otherwise the history is rebuilt in time order in a side table, in
    /// bounded batches, and swapped in by one transaction.
    fn merge_legacy_quota(
        &self,
        readings: Vec<(usize, crate::QuotaReading)>,
        summary: &mut LegacyImportSummary,
    ) -> Result<()> {
        if readings.is_empty() {
            return Ok(());
        }
        let layout = self.0.layout.as_ref().unwrap();
        let mut catalog = layout.catalog.lock().unwrap();
        catalog.execute_batch(quota::QUOTA_SCHEMA)?;
        // A side table left by an interrupted rebuild was never swapped in.
        catalog.execute_batch("DROP TABLE IF EXISTS quota_history_next;")?;
        let mut rows = Vec::new();
        let mut present = std::collections::HashSet::new();
        {
            let mut statement =
                catalog.prepare("SELECT id,data,balance FROM quota_history ORDER BY id")?;
            let existing = statement.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?;
            for row in existing {
                let (id, data, balance) = row?;
                let reading: crate::QuotaReading = serde_json::from_str(&data)?;
                present.insert(data.clone());
                rows.push(QuotaRow {
                    time: observed(&reading)?,
                    origin: QuotaOrigin::Central(id),
                    reading,
                    data,
                    balance,
                });
            }
        }
        let central = rows.len();
        for (index, mut reading) in readings {
            // The balance is a column, never part of the stored JSON.
            let balance = reading.balance.take();
            let data = serde_json::to_string(&reading)?;
            if present.insert(data.clone()) {
                rows.push(QuotaRow {
                    time: observed(&reading)?,
                    origin: QuotaOrigin::Source(index),
                    reading,
                    data,
                    balance,
                });
            }
        }
        if rows.len() == central {
            return Ok(());
        }
        // Equal times keep central rows first (by id), then sources in order.
        rows.sort_by(|left, right| {
            left.time
                .cmp(&right.time)
                .then(left.origin.cmp(&right.origin))
        });
        let mut previous: HashMap<(String, String), usize> = HashMap::new();
        let mut kept = Vec::with_capacity(rows.len());
        for (position, row) in rows.iter().enumerate() {
            let key = (row.reading.account.clone(), row.reading.window.clone());
            if let Some(&last) = previous.get(&key)
                && same_quota(&rows[last].reading, &row.reading)?
            {
                continue;
            }
            previous.insert(key, position);
            kept.push(position);
        }
        // Appending is exact when every central row survives, in id order,
        // ahead of every new row.
        let central_prefix: Vec<i64> = kept
            .iter()
            .take(central)
            .filter_map(|&position| match rows[position].origin {
                QuotaOrigin::Central(id) => Some(id),
                QuotaOrigin::Source(_) => None,
            })
            .collect();
        let append = central_prefix.len() == central
            && central_prefix.windows(2).all(|pair| pair[0] < pair[1]);
        let (table, fresh) = if append {
            ("quota_history", &kept[central..])
        } else {
            catalog.execute_batch(
                "CREATE TABLE quota_history_next(
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    provider TEXT NOT NULL, account TEXT NOT NULL, window TEXT NOT NULL,
                    data TEXT NOT NULL, balance TEXT);",
            )?;
            ("quota_history_next", &kept[..])
        };
        let insert = format!(
            "INSERT INTO {table}(provider,account,window,data,balance) VALUES(?1,?2,?3,?4,?5)"
        );
        for batch in fresh.chunks(LEGACY_IMPORT_BATCH) {
            let tx = catalog.transaction()?;
            for &position in batch {
                let row = &rows[position];
                tx.execute(
                    &insert,
                    params![
                        row.reading.provider,
                        row.reading.account,
                        row.reading.window,
                        row.data,
                        row.balance
                    ],
                )?;
            }
            tx.commit()?;
            layout.catalog_transactions.fetch_add(1, Ordering::Relaxed);
            summary.transactions += 1;
        }
        if !append {
            let tx = catalog.transaction()?;
            tx.execute_batch(
                "DROP TABLE quota_history;
                 ALTER TABLE quota_history_next RENAME TO quota_history;",
            )?;
            tx.execute_batch(quota::QUOTA_SCHEMA)?;
            tx.commit()?;
            layout.catalog_transactions.fetch_add(1, Ordering::Relaxed);
            summary.transactions += 1;
        }
        for &position in &kept {
            if let QuotaOrigin::Source(index) = rows[position].origin {
                summary.sources[index].quota_rows += 1;
            }
        }
        Ok(())
    }
}
