//! Disposable SQLite response cache, kept outside the authoritative chat record.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const APPLICATION_ID: i64 = 0x42505743; // BPWC, never open an unrelated DB as a cache.
const ACCESS_TOUCH_MS: i64 = 60_000;

fn size(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<usize> {
    let value: i64 = row.get(index)?;
    usize::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

struct Inner {
    connection: Mutex<Connection>,
    path: PathBuf,
    writes: AtomicUsize,
    maintenance_writes: AtomicUsize,
}

#[derive(Clone)]
pub(super) struct SqliteCache(Arc<Inner>);

pub(super) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

impl SqliteCache {
    /// Independently constructed clients use the same connection when paths match.
    pub(super) fn open(path: impl AsRef<Path>) -> Result<Self> {
        static OPEN: OnceLock<Mutex<HashMap<PathBuf, Weak<Inner>>>> = OnceLock::new();
        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let path = std::path::absolute(path)?;
        let mut open = OPEN
            .get_or_init(Mutex::default)
            .lock()
            .expect("cache owners poisoned");
        open.retain(|_, cache| cache.strong_count() > 0);
        if let Some(inner) = open.get(&path).and_then(Weak::upgrade) {
            return Ok(Self(inner));
        }
        let conn = Connection::open(&path).context("opening disposable web cache")?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let application: i64 = conn.pragma_query_value(None, "application_id", |row| row.get(0))?;
        let tables: usize = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| size(row, 0),
        )?;
        ensure!(
            application == APPLICATION_ID || (application == 0 && tables == 0),
            "refusing unrelated SQLite file as web cache"
        );
        // Incremental vacuum reclaims evictions; SQLite reuses pages between trims.
        conn.execute_batch("PRAGMA auto_vacuum=INCREMENTAL; PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA wal_autocheckpoint=128; PRAGMA journal_size_limit=1048576;")?;
        conn.pragma_update(None, "application_id", APPLICATION_ID)?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS responses(key TEXT PRIMARY KEY,payload BLOB NOT NULL,bytes INTEGER NOT NULL,created_ms INTEGER NOT NULL,last_access_ms INTEGER NOT NULL); CREATE INDEX IF NOT EXISTS responses_access ON responses(last_access_ms,created_ms);")?;
        let inner = Arc::new(Inner {
            connection: Mutex::new(conn),
            path: path.clone(),
            writes: AtomicUsize::new(0),
            maintenance_writes: AtomicUsize::new(0),
        });
        open.insert(path, Arc::downgrade(&inner));
        Ok(Self(inner))
    }

    pub(super) fn temporary_shared() -> Result<Self> {
        // Legacy clients have no host data directory. Process-local SQLite
        // avoids leaving temporary files after a host exits. Store/path-bound
        // clients always use their persistent web-cache.db.
        static SHARED: OnceLock<Mutex<Option<SqliteCache>>> = OnceLock::new();
        let mut shared = SHARED
            .get_or_init(Mutex::default)
            .lock()
            .expect("legacy web cache poisoned");
        if let Some(cache) = shared.as_ref() {
            return Ok(cache.clone());
        }
        let connection = Connection::open_in_memory()?;
        connection.execute_batch("CREATE TABLE responses(key TEXT PRIMARY KEY,payload BLOB NOT NULL,bytes INTEGER NOT NULL,created_ms INTEGER NOT NULL,last_access_ms INTEGER NOT NULL); CREATE INDEX responses_access ON responses(last_access_ms,created_ms);")?;
        let cache = Self(Arc::new(Inner {
            connection: Mutex::new(connection),
            path: PathBuf::from(":legacy-web-cache:"),
            writes: AtomicUsize::new(0),
            maintenance_writes: AtomicUsize::new(0),
        }));
        *shared = Some(cache.clone());
        Ok(cache)
    }

    pub(super) fn path(&self) -> &Path {
        &self.0.path
    }

    pub(super) fn lookup<T: DeserializeOwned>(
        &self,
        key: &str,
        ttl: Duration,
        max_bytes: usize,
    ) -> Result<Option<T>> {
        if ttl.is_zero() {
            return Ok(None);
        }
        let conn = self.0.connection.lock().expect("web cache poisoned");
        let row: Option<(Vec<u8>, i64, i64, usize)> = conn
            .query_row(
                "SELECT payload,created_ms,last_access_ms,bytes FROM responses WHERE key=?1",
                [key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, size(row, 3)?)),
            )
            .optional()?;
        let Some((payload, created, access, bytes)) = row else {
            return Ok(None);
        };
        let now = now_ms();
        let ttl_ms = ttl.as_millis().min(i64::MAX as u128) as i64;
        if now.saturating_sub(created) >= ttl_ms || created > now || bytes > max_bytes {
            conn.execute("DELETE FROM responses WHERE key=?1", [key])?;
            self.0.writes.fetch_add(1, Ordering::Relaxed);
            return Ok(None);
        }
        // Pagination is normally read-only: at most one access touch per minute.
        if now.saturating_sub(access) >= ACCESS_TOUCH_MS {
            conn.execute(
                "UPDATE responses SET last_access_ms=?2 WHERE key=?1",
                params![key, now],
            )?;
            self.0.writes.fetch_add(1, Ordering::Relaxed);
        }
        match serde_json::from_slice(&payload) {
            Ok(response) => Ok(Some(response)),
            Err(_) => {
                // Disposable corruption/old payload shape is a miss, never chat loss.
                conn.execute("DELETE FROM responses WHERE key=?1", [key])?;
                self.0.writes.fetch_add(1, Ordering::Relaxed);
                Ok(None)
            }
        }
    }

    pub(super) fn insert<T: Serialize>(
        &self,
        key: &str,
        value: &T,
        ttl: Duration,
        max_bytes: usize,
    ) -> Result<()> {
        if ttl.is_zero() {
            return Ok(());
        }
        let payload = serde_json::to_vec(value)?;
        let bytes = payload.len().saturating_add(key.len());
        let mut conn = self.0.connection.lock().expect("web cache poisoned");
        let tx = conn.transaction()?;
        let now = now_ms();
        let cutoff = now.saturating_sub(ttl.as_millis().min(i64::MAX as u128) as i64);
        let mut evicted = tx.execute("DELETE FROM responses WHERE created_ms<=?1", [cutoff])? > 0;
        if bytes <= max_bytes {
            tx.execute("INSERT OR REPLACE INTO responses(key,payload,bytes,created_ms,last_access_ms) VALUES(?1,?2,?3,?4,?4)", params![key,payload,i64::try_from(bytes)?,now])?;
        }
        let mut total: usize =
            tx.query_row("SELECT coalesce(sum(bytes),0) FROM responses", [], |row| {
                size(row, 0)
            })?;
        while total > max_bytes {
            let (old_key, old_bytes): (String, usize) = tx.query_row(
                "SELECT key,bytes FROM responses ORDER BY last_access_ms,created_ms,key LIMIT 1",
                [],
                |row| Ok((row.get(0)?, size(row, 1)?)),
            )?;
            tx.execute("DELETE FROM responses WHERE key=?1", [old_key])?;
            evicted = true;
            total = total.saturating_sub(old_bytes);
        }
        tx.commit()?;
        self.0.writes.fetch_add(1, Ordering::Relaxed);
        if evicted {
            self.reclaim(&conn)?;
        }
        Ok(())
    }

    /// SQLite must not run on a runtime worker: the blocking pool owns it.
    pub(super) async fn lookup_async<T: DeserializeOwned + Send + 'static>(
        &self,
        key: &str,
        ttl: Duration,
        max_bytes: usize,
    ) -> Result<Option<T>> {
        let cache = self.clone();
        let key = key.to_owned();
        tokio::task::spawn_blocking(move || cache.lookup(&key, ttl, max_bytes))
            .await
            .context("web cache lookup task stopped")?
    }

    /// The same insert off the runtime worker; the value moves to that thread.
    pub(super) async fn insert_async<T: Serialize + Send + 'static>(
        &self,
        key: &str,
        value: T,
        ttl: Duration,
        max_bytes: usize,
    ) -> Result<()> {
        let cache = self.clone();
        let key = key.to_owned();
        tokio::task::spawn_blocking(move || cache.insert(&key, &value, ttl, max_bytes))
            .await
            .context("web cache insert task stopped")?
    }

    fn reclaim(&self, conn: &Connection) -> Result<()> {
        // Consume every row: only stepping once can leave free pages unreclaimed.
        let mut statement = conn.prepare("PRAGMA incremental_vacuum")?;
        let mut rows = statement.query([])?;
        while rows.next()?.is_some() {}
        drop(rows);
        drop(statement);
        self.0.maintenance_writes.fetch_add(1, Ordering::Relaxed);
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        Ok(())
    }

    /// A reduced configured cap applies immediately to an existing shared cache.
    pub(super) fn enforce_limit(&self, max_bytes: usize) -> Result<()> {
        let mut conn = self.0.connection.lock().expect("web cache poisoned");
        let mut total: usize =
            conn.query_row("SELECT coalesce(sum(bytes),0) FROM responses", [], |row| {
                size(row, 0)
            })?;
        if total <= max_bytes {
            return Ok(());
        }
        let tx = conn.transaction()?;
        while total > max_bytes {
            let (key, bytes): (String, usize) = tx.query_row(
                "SELECT key,bytes FROM responses ORDER BY last_access_ms,created_ms,key LIMIT 1",
                [],
                |row| Ok((row.get(0)?, size(row, 1)?)),
            )?;
            tx.execute("DELETE FROM responses WHERE key=?1", [key])?;
            total = total.saturating_sub(bytes);
        }
        tx.commit()?;
        self.0.writes.fetch_add(1, Ordering::Relaxed);
        self.reclaim(&conn)?;
        Ok(())
    }

    pub(super) fn checkpoint(&self) -> Result<()> {
        self.0
            .connection
            .lock()
            .expect("web cache poisoned")
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn measurements(&self) -> Result<(usize, u64, u64, usize, usize)> {
        let conn = self.0.connection.lock().expect("web cache poisoned");
        let bytes = conn.query_row("SELECT coalesce(sum(bytes),0) FROM responses", [], |row| {
            size(row, 0)
        })?;
        let disk = std::fs::metadata(&self.0.path)?.len();
        let wal =
            std::fs::metadata(format!("{}-wal", self.0.path.display())).map_or(0, |m| m.len());
        Ok((
            bytes,
            disk,
            wal,
            self.0.writes.load(Ordering::Relaxed),
            self.0.maintenance_writes.load(Ordering::Relaxed),
        ))
    }
}

/// Coordination contains no response data and is shared across clients.
pub(super) fn flight(path: &Path, key: &str) -> Arc<tokio::sync::Mutex<()>> {
    type Flights = HashMap<(PathBuf, String), Weak<tokio::sync::Mutex<()>>>;
    static FLIGHTS: OnceLock<Mutex<Flights>> = OnceLock::new();
    let mut flights = FLIGHTS
        .get_or_init(Mutex::default)
        .lock()
        .expect("web flights poisoned");
    flights.retain(|_, flight| flight.strong_count() > 0);
    let key = (path.to_owned(), key.to_owned());
    if let Some(flight) = flights.get(&key).and_then(Weak::upgrade) {
        return flight;
    }
    let flight = Arc::new(tokio::sync::Mutex::new(()));
    flights.insert(key, Arc::downgrade(&flight));
    flight
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistence_ttl_eviction_and_unrelated_database_safety() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("web-cache.db");
        let ttl = Duration::from_secs(86_400);
        let cache = SqliteCache::open(&path).unwrap();
        cache.insert("first", &"a".repeat(500), ttl, 1024).unwrap();
        cache.insert("second", &"b".repeat(500), ttl, 1024).unwrap();
        cache.insert("third", &"c".repeat(500), ttl, 1024).unwrap();
        assert!(
            cache
                .lookup::<String>("first", ttl, 1024)
                .unwrap()
                .is_none()
        );
        assert!(
            cache
                .lookup::<String>("third", ttl, 1024)
                .unwrap()
                .is_some()
        );
        assert!(cache.measurements().unwrap().0 <= 1024);
        drop(cache);
        let cache = SqliteCache::open(&path).unwrap();
        assert!(
            cache
                .lookup::<String>("third", ttl, 1024)
                .unwrap()
                .is_some()
        );
        cache
            .0
            .connection
            .lock()
            .unwrap()
            .execute("UPDATE responses SET created_ms=0", [])
            .unwrap();
        assert!(
            cache
                .lookup::<String>("third", ttl, 1024)
                .unwrap()
                .is_none()
        );
        drop(cache);
        std::fs::remove_file(&path).unwrap();
        assert!(
            SqliteCache::open(&path)
                .unwrap()
                .lookup::<String>("third", ttl, 1024)
                .unwrap()
                .is_none()
        );
        let other = directory.path().join("record.db");
        let conn = Connection::open(&other).unwrap();
        conn.execute("CREATE TABLE important(value TEXT)", [])
            .unwrap();
        assert!(SqliteCache::open(&other).is_err());
        assert_eq!(
            conn.query_row(
                "SELECT name FROM sqlite_master WHERE type='table'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "important"
        );
    }

    #[test]
    fn repeated_eviction_reclaims_disk_and_lowered_caps_apply_on_open() {
        let directory = tempfile::tempdir().unwrap();
        let cache = SqliteCache::open(directory.path().join("web-cache.db")).unwrap();
        let ttl = Duration::from_secs(86_400);
        for number in 0..50 {
            cache
                .insert(
                    &format!("response-{number:03}"),
                    &"x".repeat(24 * 1024),
                    ttl,
                    64 * 1024,
                )
                .unwrap();
            let stats = cache.measurements().unwrap();
            assert!(stats.0 <= 64 * 1024);
            // SQLite's schema/index/page floor sits outside the serialized cap.
            assert!(stats.1 + stats.2 <= 256 * 1024, "{stats:?}");
        }
        cache.checkpoint().unwrap();
        let stats = cache.measurements().unwrap();
        assert!(stats.1 <= 96 * 1024, "{stats:?}");
        assert_eq!(stats.2, 0);
        cache.enforce_limit(1024).unwrap();
        let stats = cache.measurements().unwrap();
        assert_eq!(stats.0, 0);
        assert!(stats.1 <= 32 * 1024, "{stats:?}");
        assert_eq!(stats.2, 0);
    }
}
