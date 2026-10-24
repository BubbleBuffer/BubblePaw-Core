//! Legacy sources of the central store beyond its own siblings: previous
//! default stores (`BubblePaw-base/store`, the location before the rename to
//! `BubblePaw-Core`) and the per-workspace stores beside them.
//!
//! A previous default store is imported as one source, through the same
//! `import_candidates` path as any other. Its catalog receipts name the
//! per-workspace stores it already absorbed; those receipts are copied here
//! together with its own import, so those stores are not imported twice.
use super::*;
use std::collections::HashSet;

/// Receipt name, directory, and whether the source still needs an import.
type Candidate = (String, PathBuf, bool);

/// Receipt name of a previous default store, `central:<parent>/<name>`. It
/// cannot collide with a 64-hex per-workspace name.
fn previous_name(store: &Path) -> String {
    let names: Vec<String> = store
        .iter()
        .rev()
        .take(2)
        .map(|part| part.to_string_lossy().into_owned())
        .collect();
    format!(
        "central:{}",
        names.into_iter().rev().collect::<Vec<_>>().join("/")
    )
}

/// Receipts a staged previous store recorded for its own per-workspace
/// sources, except those recorded as unsupported (they still need importing).
fn absorbed_receipts(catalog: &Path) -> Result<Vec<(String, String)>> {
    let conn = Connection::open(catalog)?;
    let mut statement =
        conn.prepare("SELECT key, value FROM metadata WHERE key LIKE 'legacy_import:%'")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut receipts = Vec::new();
    for row in rows {
        let (key, value) = row?;
        let Some(name) = key.strip_prefix(MARKER_PREFIX) else {
            continue;
        };
        let unsupported = serde_json::from_str::<serde_json::Value>(&value)
            .is_ok_and(|receipt| receipt.get("unsupported").is_some());
        if is_legacy_store_name(name) && !unsupported {
            receipts.push((key, value));
        }
    }
    Ok(receipts)
}

impl Store {
    /// Whether a source with this receipt name still needs its import. A
    /// recorded source is done, unless it was recorded as unsupported and has
    /// since been migrated into a directory store.
    fn legacy_source_pending(&self, name: &str, dir: &Path) -> Result<bool> {
        let layout = self
            .0
            .layout
            .as_ref()
            .context("Legacy import requires a directory store")?;
        let receipt = metadata_value(
            &layout.catalog.lock().unwrap(),
            &format!("{MARKER_PREFIX}{name}"),
        )?;
        Ok(match receipt {
            None => true,
            Some(receipt) => {
                serde_json::from_str::<serde_json::Value>(&receipt)
                    .is_ok_and(|receipt| receipt.get("unsupported").is_some())
                    && dir.join("catalog.db").is_file()
            }
        })
    }

    /// Per-workspace stores (64-hex directory names) directly inside `parent`.
    fn legacy_siblings(&self, parent: &Path) -> Result<Vec<Candidate>> {
        let layout = self
            .0
            .layout
            .as_ref()
            .context("Legacy import requires a directory store")?;
        let mut candidates = Vec::new();
        if !parent.is_dir() {
            return Ok(candidates);
        }
        for entry in std::fs::read_dir(parent)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_legacy_store_name(&name)
                || !entry.file_type()?.is_dir()
                || entry.path() == layout.data_dir
            {
                continue;
            }
            let pending = self.legacy_source_pending(&name, &entry.path())?;
            candidates.push((name, entry.path(), pending));
        }
        Ok(candidates)
    }

    /// The previous default stores that exist, each with its pending state.
    fn previous_sources(&self, previous: &[PathBuf]) -> Result<Vec<Candidate>> {
        let mut sources = Vec::new();
        for store in previous.iter().filter(|store| store.is_dir()) {
            let name = previous_name(store);
            let pending = self.legacy_source_pending(&name, store)?;
            sources.push((name, store.clone(), pending));
        }
        Ok(sources)
    }

    /// Per-workspace stores beside this store and beside each previous store,
    /// one entry per name, sorted.
    fn sibling_sources(&self, previous: &[PathBuf]) -> Result<Vec<Candidate>> {
        let layout = self
            .0
            .layout
            .as_ref()
            .context("Legacy import requires a directory store")?;
        let mut parents: Vec<PathBuf> = layout
            .data_dir
            .parent()
            .map(Path::to_path_buf)
            .into_iter()
            .collect();
        parents.extend(
            previous
                .iter()
                .filter_map(|store| store.parent().map(Path::to_path_buf)),
        );
        let mut seen = HashSet::new();
        let mut candidates = Vec::new();
        for parent in parents {
            for candidate in self.legacy_siblings(&parent)? {
                if seen.insert(candidate.0.clone()) {
                    candidates.push(candidate);
                }
            }
        }
        candidates.sort();
        Ok(candidates)
    }

    /// Whether a legacy sibling store still waits for its import. Cheap: it
    /// reads directory names and catalog receipts only.
    pub fn legacy_import_pending(&self) -> Result<bool> {
        self.legacy_import_pending_with(&[])
    }

    /// `legacy_import_pending`, also counting the `previous` default stores and
    /// the per-workspace stores beside them.
    pub fn legacy_import_pending_with(&self, previous: &[PathBuf]) -> Result<bool> {
        let previous_sources = self.previous_sources(previous)?;
        let siblings = self.sibling_sources(previous)?;
        Ok(previous_sources
            .iter()
            .chain(&siblings)
            .any(|(_, _, pending)| *pending))
    }

    /// Import every legacy source: first the `previous` default stores, then
    /// the per-workspace stores beside this store and beside them. The
    /// contract is that of `import_legacy_stores`.
    ///
    /// A previous store that imports also records the per-workspace stores its
    /// own catalog absorbed, in the same pass, so they are skipped below.
    pub fn import_legacy_stores_with(
        &self,
        previous: &[PathBuf],
        mut merge: impl FnMut(&LegacySnapshot) -> Result<()>,
    ) -> Result<LegacyImportSummary> {
        let mut summary = LegacyImportSummary::default();
        let previous_sources = self.previous_sources(previous)?;
        if !previous_sources.is_empty() {
            let mut absorbed = 0_u64;
            let part = self.import_candidates(previous_sources, |snapshot| {
                absorbed += self.record_absorbed(snapshot)?;
                merge(snapshot)
            })?;
            summary.sources.extend(part.sources);
            summary.transactions += part.transactions + absorbed;
        }
        let siblings = self.sibling_sources(previous)?;
        let part = self.import_candidates(siblings, &mut merge)?;
        summary.sources.extend(part.sources);
        summary.transactions += part.transactions;
        Ok(summary)
    }

    /// Copy the absorbed receipts of one staged previous store into this
    /// catalog. Called before the store's own merge; if that merge or the
    /// receipt write fails, the previous store is retried and re-installs its
    /// chats, so nothing is lost. Returns the transactions written (0 or 1).
    fn record_absorbed(&self, snapshot: &LegacySnapshot) -> Result<u64> {
        let receipts = absorbed_receipts(&snapshot.dir.join("catalog.db"))?;
        if receipts.is_empty() {
            return Ok(0);
        }
        let layout = self
            .0
            .layout
            .as_ref()
            .context("Legacy import requires a directory store")?;
        let mut catalog = layout.catalog.lock().unwrap();
        let tx = catalog.transaction()?;
        for (key, receipt) in receipts {
            tx.execute(
                "INSERT OR IGNORE INTO metadata VALUES(?1,?2)",
                params![key, receipt],
            )?;
        }
        tx.commit()?;
        layout.catalog_transactions.fetch_add(1, Ordering::Relaxed);
        Ok(1)
    }
}
