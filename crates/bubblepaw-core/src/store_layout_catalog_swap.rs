//! The final step of a catalog rebuild. The scan reads chat files one by one,
//! so sessions committed meanwhile must survive the swap: only rows that existed
//! before the scan are replaced, and the pending buffer is applied over the scan.
use super::*;
use std::collections::HashSet;

type Scanned = (String, Session, String);

/// Root-level metadata the scan collected alongside the sessions.
pub(super) struct ScannedRoots<'a> {
    pub storage: HashMap<String, Option<RootStoragePolicy>>,
    pub archive_times: HashMap<String, String>,
    pub archived: &'a HashSet<String>,
}

impl Store {
    /// The ids of every catalogued session, taken before the scan starts.
    pub(super) fn catalog_ids(layout: &Layout) -> Result<Vec<String>> {
        let catalog = layout.catalog.lock().unwrap();
        let mut statement = catalog.prepare("SELECT id FROM sessions")?;
        Ok(statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Replace the rows that existed before the scan with the scan, then apply
    /// the pending buffer. Returns the sessions committed after the scan began.
    /// The buffer is not held across the transaction (a writer holding SQLite's
    /// lock may be waiting for it); an entry that changed meanwhile stays pending.
    pub(super) fn swap_catalog(
        layout: &Layout,
        rows_at_start: &[String],
        records: &[Scanned],
        search_targets: &HashMap<String, i64>,
        roots: ScannedRoots,
    ) -> Result<Vec<Scanned>> {
        let pending: Vec<_> = layout
            .pending_catalog
            .lock()
            .unwrap()
            .iter()
            .map(|(id, (root, session, activity))| {
                (id.clone(), root.clone(), session.clone(), activity.clone())
            })
            .collect();
        let mut catalog = layout.catalog.lock().unwrap();
        let tx = catalog.transaction()?;
        for id in rows_at_start {
            tx.execute("DELETE FROM sessions WHERE id=?1", [id])?;
        }
        tx.execute(
            "DELETE FROM metadata WHERE substr(key,1,13)='root_storage:'",
            [],
        )?;
        for (root, policy) in roots.storage {
            tx.execute(
                "INSERT INTO metadata VALUES(?1,?2)",
                params![
                    format!("root_storage:{root}"),
                    serde_json::to_string(&policy)?
                ],
            )?;
        }
        for (root, time) in roots.archive_times {
            tx.execute(
                "INSERT OR REPLACE INTO metadata VALUES(?1,?2)",
                params![format!("archived_at:{root}"), time],
            )?;
        }
        tx.execute(
            "DELETE FROM metadata WHERE substr(key,1,14)='search_target:'",
            [],
        )?;
        for (root, session, activity) in records {
            Self::catalog_session(&tx, root, session, activity)?;
            if let Some(target) = search_targets.get(&session.id) {
                tx.execute(
                    "INSERT INTO metadata VALUES(?1,?2)",
                    params![format!("search_target:{}", session.id), target.to_string()],
                )?;
            }
            if roots.archived.contains(root) {
                tx.execute(
                    "UPDATE sessions SET storage_state='archived' WHERE id=?1",
                    [&session.id],
                )?;
            }
        }
        for (_, root, session, activity) in &pending {
            Self::catalog_session(&tx, root, session, activity)?;
        }
        let scanned: HashSet<&str> = records.iter().map(|(_, s, _)| s.id.as_str()).collect();
        let mut newer = Vec::new();
        {
            let mut statement = tx.prepare("SELECT root_id,data,last_activity FROM sessions")?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get(2)?,
                ))
            })?;
            for row in rows {
                let (root, data, activity) = row?;
                let session: Session = serde_json::from_str(&data)?;
                if !scanned.contains(session.id.as_str()) {
                    newer.push((root, session, activity));
                }
            }
        }
        tx.execute("DELETE FROM metadata WHERE substr(key,1,12)='archived_at:' AND NOT EXISTS(SELECT 1 FROM sessions WHERE sessions.id=substr(metadata.key,13) AND sessions.id=sessions.root_id)", [])?;
        tx.commit()?;
        layout.catalog_transactions.fetch_add(1, Ordering::Relaxed);
        drop(catalog);
        let same = |id: &str, session: &Session, activity: &str| {
            pending.iter().any(|(pid, _, psession, pactivity)| {
                pid == id
                    && pactivity == activity
                    && serde_json::to_string(psession).ok() == serde_json::to_string(session).ok()
            })
        };
        layout
            .pending_catalog
            .lock()
            .unwrap()
            .retain(|id, (_, session, activity)| !same(id, session, activity));
        Ok(newer)
    }
}
