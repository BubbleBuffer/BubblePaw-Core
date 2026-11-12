//! Chat file lookup, open and bootstrap.
use super::*;

impl Store {
    pub fn data_dir(&self) -> Option<&Path> {
        self.0
            .layout
            .as_ref()
            .map(|layout| layout.data_dir.as_path())
    }
    pub(in crate::store) fn host(&self) -> Option<Self> {
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
    pub(super) fn open_chat(&self, root: &str) -> Result<Self> {
        self.open_chat_mode(root, false)
    }
    pub(in crate::store) fn bootstrap_chat(&self, root: &str) -> Result<Self> {
        self.open_chat_mode(root, true)
    }
    fn open_chat_mode(&self, root: &str, create: bool) -> Result<Self> {
        self.open_chat_file(root, &format!("chats/{root}.db"), create)
    }
    pub(super) fn open_chat_file(&self, root: &str, file: &str, create: bool) -> Result<Self> {
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
        release::ensure_not_deleting(layout, root)?;
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
        let connection = open_record(&path, false, false)?.into_connection();
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
            anyhow::ensure!(application == 0x42504230, "Unsupported chat record schema");
            StoreSchemaError::ensure("chat", version, schema::CHAT_VERSION)?;
            connection.busy_timeout(std::time::Duration::from_secs(2))?;
            connection.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA wal_autocheckpoint=256; PRAGMA journal_size_limit=1048576;")?;
            Self::initialize_reminders(&connection)?; // existing chat-v1 records skip initialize
            cold::initialize(&connection)?;
            let dictionary_pins = cold::pin_dictionaries(&connection)?;
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
}
