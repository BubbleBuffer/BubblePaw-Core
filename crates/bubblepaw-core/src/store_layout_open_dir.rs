//! Opening a layout data directory, including the legacy import.
use super::*;

impl Store {
    /// The `<stem>.data` directory beside a single-file path, which `Store::open` uses.
    pub fn legacy_data_dir(path: impl AsRef<Path>) -> PathBuf {
        let path = path.as_ref();
        path.with_file_name(format!(
            "{}.data",
            path.file_stem()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
        ))
    }
    pub(super) fn copy_rows(
        source: &Connection,
        destination: &Connection,
        table: &str,
        filter: &str,
        ids: &[String],
    ) -> Result<()> {
        let mut query = source.prepare(&format!("SELECT * FROM {table}{filter}"))?;
        let columns = query
            .column_names()
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
        let catalog = projection(
            &data_dir.join("catalog.db"),
            0x42504231,
            CATALOG_SCHEMA,
            "catalog",
            schema::CATALOG_VERSION,
        )?;
        let serving = projection(
            &data_dir.join("serving.db"),
            0x42504232,
            SERVING_SCHEMA,
            "serving",
            schema::SERVING_VERSION,
        )?;
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
                deleting: Mutex::default(),
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
}
