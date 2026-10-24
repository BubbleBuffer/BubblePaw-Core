//! Root-chat source IDs. Only durable metadata enters the authoritative store;
//! fetched page text remains in the separate disposable web cache.
use anyhow::{Result, anyhow, ensure};
use bubblepaw_core::Store;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, OnceLock},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub id: String,
    pub url: String,
    pub title: String,
    pub backend: String,
}

#[derive(Default)]
struct Index {
    by_url: HashMap<String, Source>,
    next: usize,
}

#[derive(Clone)]
pub(super) struct SourceStore {
    store: Store,
    indexes: Arc<Mutex<HashMap<String, Index>>>,
}

impl SourceStore {
    pub(super) fn new(store: &Store) -> Self {
        Self {
            store: store.clone(),
            indexes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn root(&self, session: &str) -> Result<String> {
        let mut current = session.to_owned();
        let mut visited = HashSet::new();
        loop {
            ensure!(
                visited.insert(current.clone()),
                "cycle in web source session ancestry"
            );
            match self.store.session(&current)?.parent_session_id {
                Some(parent) => current = parent,
                None => return Ok(current),
            }
        }
    }

    fn read(&self, root: &str, number: usize) -> Result<Option<Source>> {
        self.store
            .attachment(root, &format!("web-source:s{number}"))?
            .map(|(_, bytes)| serde_json::from_slice(&bytes).map_err(Into::into))
            .transpose()
    }

    pub(super) fn lookup(&self, session: &str, id: &str) -> Result<Source> {
        let number = id
            .strip_prefix('s')
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n > 0 && id == format!("s{n}"))
            .ok_or_else(|| anyhow!("source id must be s followed by a positive integer"))?;
        self.read(&self.root(session)?, number)?
            .ok_or_else(|| anyhow!("unknown web source id {id}"))
    }

    pub(super) fn allocate(
        &self,
        session: &str,
        url: &str,
        title: &str,
        backend: &str,
    ) -> Result<Source> {
        let root = self.root(session)?;
        let mut url = super::checked_url(url)?;
        url.set_fragment(None);
        let url = url.to_string();
        // Store attachments are immutable. Serialize allocation across registries
        // and refresh the tail written by other registries before choosing an ID.
        static ALLOCATION: OnceLock<Mutex<()>> = OnceLock::new();
        let _allocation = ALLOCATION
            .get_or_init(Mutex::default)
            .lock()
            .expect("web source allocation poisoned");
        let mut indexes = self.indexes.lock().expect("web source index poisoned");
        let index = indexes.entry(root.clone()).or_insert_with(|| Index {
            next: 1,
            ..Default::default()
        });
        while let Some(source) = self.read(&root, index.next)? {
            index.by_url.insert(source.url.clone(), source);
            index.next += 1;
        }
        if let Some(source) = index.by_url.get(&url) {
            return Ok(source.clone());
        }
        let source = Source {
            id: format!("s{}", index.next),
            url: url.clone(),
            title: title.chars().take(1000).collect(),
            backend: backend.to_owned(),
        };
        self.store.store_attachment(
            &root,
            &format!("web-source:{}", source.id),
            &serde_json::json!({"type":"web_source","source_id":source.id}).to_string(),
            &serde_json::to_vec(&source)?,
        )?;
        index.next += 1;
        index.by_url.insert(url, source.clone());
        Ok(source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_and_children_share_ids_across_registries_and_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open_data_dir(directory.path()).unwrap();
        let parent = store.create_session("root").unwrap();
        let child = store
            .create_session_with_parent("child", Default::default(), Some(&parent.id))
            .unwrap();
        let first = SourceStore::new(&store);
        let second = SourceStore::new(&store);
        let source = first
            .allocate(&child.id, "https://example.org/a#anchor", "A", "direct")
            .unwrap();
        assert_eq!(source.id, "s1");
        assert_eq!(
            second
                .allocate(&parent.id, "https://example.org/a", "new title", "direct")
                .unwrap()
                .id,
            "s1"
        );
        assert_eq!(
            second
                .allocate(&parent.id, "https://example.org/b", "B", "direct")
                .unwrap()
                .id,
            "s2"
        );
        assert_eq!(
            first.lookup(&parent.id, "s1").unwrap().url,
            "https://example.org/a"
        );
        assert!(first.lookup(&parent.id, "s01").is_err());
        drop(first);
        drop(second);
        drop(store);
        let store = Store::open_data_dir(directory.path()).unwrap();
        let restored = SourceStore::new(&store);
        assert_eq!(restored.lookup(&child.id, "s2").unwrap().title, "B");
        assert_eq!(
            restored
                .allocate(&parent.id, "https://example.org/c", "C", "direct")
                .unwrap()
                .id,
            "s3"
        );
    }
}
