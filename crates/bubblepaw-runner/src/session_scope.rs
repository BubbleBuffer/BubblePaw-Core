//! Request ownership within a host's shared event and runtime state.
use bubblepaw_core::Store;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Ownership {
    sessions: std::collections::HashMap<String, uuid::Uuid>,
    held_roots: std::collections::HashMap<String, uuid::Uuid>,
}

#[derive(Debug)]
pub struct SessionLeaseBusy(pub String);
impl std::fmt::Display for SessionLeaseBusy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Session {} belongs to another active request",
            self.0
        )
    }
}
impl std::error::Error for SessionLeaseBusy {}

#[derive(Clone)]
pub struct SessionScope {
    owners: Arc<Mutex<Ownership>>,
    owner: uuid::Uuid,
    all: bool,
}

pub struct ScopeRelease(SessionScope);
impl Drop for ScopeRelease {
    fn drop(&mut self) {
        self.0.release();
    }
}

impl Default for SessionScope {
    fn default() -> Self {
        Self {
            owners: Arc::default(),
            owner: uuid::Uuid::new_v4(),
            all: false,
        }
    }
}

impl SessionScope {
    pub fn all() -> Self {
        Self {
            all: true,
            ..Self::default()
        }
    }

    /// Independent request ownership using the same host admission table.
    pub fn fork(&self) -> Self {
        Self {
            owners: self.owners.clone(),
            owner: uuid::Uuid::new_v4(),
            all: false,
        }
    }

    /// Hold a freshly created root for a one-shot request, including its cleanup.
    /// Existing sessions must use `admit` so admission and ownership are atomic.
    pub fn set_root(&self, root: &str) {
        let mut owners = self.owners.lock().unwrap();
        owners.sessions.insert(root.to_owned(), self.owner);
        owners.held_roots.insert(root.to_owned(), self.owner);
    }

    pub fn adopt_idle_root(&self, root: &str) {
        self.owners
            .lock()
            .unwrap()
            .sessions
            .entry(root.to_owned())
            .or_insert(self.owner);
    }

    fn ancestry(store: &Store, session: &str) -> anyhow::Result<Vec<String>> {
        let root = store.session_root_id(session)?;
        let mut ancestry = Vec::new();
        let mut current = session.to_owned();
        loop {
            anyhow::ensure!(
                !ancestry.contains(&current),
                "Session ancestry contains a cycle"
            );
            ancestry.push(current.clone());
            if current == root {
                return Ok(ancestry);
            }
            let metadata = match store.active_session_metadata(&current)? {
                Some(metadata) => metadata,
                None => store
                    .list_sessions()?
                    .into_iter()
                    .find(|session| session.id == current)
                    .ok_or_else(|| anyhow::anyhow!("Unknown session: {current}"))?,
            };
            // Worktree inheritance may follow a legacy cross-file parent;
            // ownership follows this chat's authoritative physical placement.
            let Some(parent) = &metadata.parent_session_id else {
                break;
            };
            if store.session_root_id(parent)? != root {
                break;
            }
            current = parent.clone();
        }
        ancestry.push(root);
        Ok(ancestry)
    }

    /// One-shot callers hold the whole root through cleanup. Long-lived clients
    /// only change event ownership after admission; their idle sessions stay
    /// attachable. The lock also keeps listeners from seeing admission events
    /// before their new owner is published. Failed admission changes nothing.
    pub fn admit<T>(
        &self,
        store: &Store,
        session: &str,
        hold: bool,
        start: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.admit_inner(store, session, hold, false, start)
    }

    /// Automatic work must still belong to this scope at the instant it starts.
    pub fn admit_owned<T>(
        &self,
        store: &Store,
        session: &str,
        start: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.admit_inner(store, session, false, true, start)
    }

    fn admit_inner<T>(
        &self,
        store: &Store,
        session: &str,
        hold: bool,
        require_owner: bool,
        start: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let ancestry = Self::ancestry(store, session)?;
        let root = ancestry.last().expect("session ancestry has a root");
        let mut owners = self.owners.lock().unwrap();
        let leased_elsewhere = owners
            .held_roots
            .get(root)
            .is_some_and(|owner| *owner != self.owner);
        let owned = self.all
            || ancestry
                .iter()
                .find_map(|ancestor| owners.sessions.get(ancestor))
                .is_some_and(|owner| *owner == self.owner);
        if leased_elsewhere || (require_owner && !owned) {
            return Err(SessionLeaseBusy(session.to_owned()).into());
        }
        // Long-lived clients release their admission lease immediately. Their
        // running descendants still own that tree until the terminal checkpoint;
        // an idle parent must not hand those events to a different configuration.
        for (candidate, owner) in &owners.sessions {
            if *owner != self.owner
                && Self::ancestry(store, candidate)
                    .is_ok_and(|ancestry| ancestry.last() == Some(root))
                && store
                    .snapshot(candidate, 0, 1, 0)?
                    .runs
                    .last()
                    .is_some_and(|run| run.status == "running")
            {
                return Err(SessionLeaseBusy(candidate.clone()).into());
            }
        }
        let result = start()?;
        // A root resumed by another request also takes its descendants back
        // from earlier idle owners; a child admission leaves siblings alone.
        let stale = owners
            .sessions
            .keys()
            .filter(|candidate| {
                candidate.as_str() != session
                    && Self::ancestry(store, candidate)
                        .is_ok_and(|ancestry| ancestry.iter().any(|ancestor| ancestor == session))
            })
            .cloned()
            .collect::<Vec<_>>();
        for stale in stale {
            owners.sessions.remove(&stale);
        }
        owners.sessions.insert(session.to_owned(), self.owner);
        if hold {
            owners.held_roots.insert(root.clone(), self.owner);
        }
        Ok(result)
    }

    pub fn release(&self) {
        let mut owners = self.owners.lock().unwrap();
        owners.sessions.retain(|_, owner| *owner != self.owner);
        owners.held_roots.retain(|_, owner| *owner != self.owner);
    }

    pub fn release_on_drop(&self) -> ScopeRelease {
        ScopeRelease(self.clone())
    }

    /// Reserve an owned root before teardown calls cancellation hooks. Keep the
    /// lease until release(), without holding this mutex across registry hooks.
    pub fn hold_owned_for_cleanup(&self, store: &Store, session: &str) -> anyhow::Result<bool> {
        let ancestry = Self::ancestry(store, session)?;
        let root = ancestry.last().expect("session ancestry has a root");
        let mut owners = self.owners.lock().unwrap();
        if owners
            .held_roots
            .get(root)
            .is_some_and(|owner| *owner != self.owner)
        {
            return Ok(false);
        }
        let owned = self.all
            || ancestry
                .iter()
                .find_map(|ancestor| owners.sessions.get(ancestor))
                .is_some_and(|owner| *owner == self.owner);
        if owned {
            owners.held_roots.insert(root.clone(), self.owner);
        }
        Ok(owned)
    }

    pub fn contains(&self, store: &Store, session: &str) -> bool {
        if self.all {
            return true;
        }
        let owners = self.owners.lock().unwrap().sessions.clone();
        if owners.is_empty() {
            return false;
        }
        if let Some(owner) = owners.get(session) {
            return *owner == self.owner;
        }
        Self::ancestry(store, session)
            .ok()
            .and_then(|ancestry| {
                ancestry
                    .iter()
                    .find_map(|ancestor| owners.get(ancestor))
                    .copied()
            })
            .is_some_and(|owner| owner == self.owner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_cross_file_parent_does_not_transfer_ownership_or_restore_archives()
    -> anyhow::Result<()> {
        let data = tempfile::tempdir()?;
        let workspace_a = tempfile::tempdir()?;
        let workspace_b = tempfile::tempdir()?;
        let store = Store::open_data_dir(data.path())?;
        let a = store.create_session_with_workspace("a", workspace_a.path())?;
        let mut b = store.create_session_with_workspace("b", workspace_b.path())?;
        let child = store.create_session_with_parent("child", Default::default(), Some(&b.id))?;
        let path_a = store.chat_path(&a.id)?.unwrap();
        let path_b = store.chat_path(&b.id)?.unwrap();
        drop(store);
        b.parent_session_id = Some(a.id.clone());
        let connection = rusqlite::Connection::open(&path_b)?;
        connection.execute(
            "UPDATE sessions SET data=?2 WHERE id=?1",
            rusqlite::params![b.id, serde_json::to_string(&b)?],
        )?;
        connection.execute(
            "DELETE FROM metadata WHERE key=?1",
            [format!("root_workspace:{}", b.id)],
        )?;
        connection.execute(
            "INSERT INTO metadata VALUES('workspace',?1)",
            [workspace_b.path().canonicalize()?.to_string_lossy()],
        )?;
        connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        drop(connection);
        let before = std::fs::read(&path_b)?;
        let store = Store::open_data_dir(data.path())?;
        let primary = SessionScope::default();
        let client = primary.fork();
        primary.set_root(&a.id);
        assert!(!primary.contains(&store, &b.id));
        assert!(!primary.contains(&store, &child.id));
        assert_eq!(store.session_root_id(&child.id)?, b.id);
        assert!(store.archive_chat(&a.id)?);
        assert!(!path_a.exists());
        client.admit(&store, &b.id, true, || Ok(()))?;
        assert!(client.contains(&store, &child.id));
        assert!(
            !path_a.exists(),
            "ownership scans must leave unrelated archives packed"
        );
        assert_eq!(
            store.session_workspace(&child.id)?,
            Some(workspace_b.path().canonicalize()?)
        );
        drop(store);
        assert_eq!(std::fs::read(path_b)?, before);
        Ok(())
    }

    #[test]
    fn cleanup_reservation_prevents_cancelling_a_successor() -> anyhow::Result<()> {
        let store = Store::in_memory()?;
        let root = store.create_session("root")?;
        let primary = SessionScope::default();
        let client = primary.fork();
        primary.adopt_idle_root(&root.id);
        assert!(primary.hold_owned_for_cleanup(&store, &root.id)?);
        assert!(client.admit(&store, &root.id, true, || Ok(())).is_err());
        primary.release();
        client.admit(&store, &root.id, true, || Ok(()))?;
        assert!(!primary.hold_owned_for_cleanup(&store, &root.id)?);
        assert!(client.contains(&store, &root.id));
        Ok(())
    }
    struct PendingProvider;
    #[async_trait::async_trait]
    impl bubblepaw_core::Provider for PendingProvider {
        fn models(&self) -> Vec<bubblepaw_core::ModelInfo> {
            vec![]
        }
        async fn generate(
            &self,
            _request: bubblepaw_core::ModelRequest,
            _text: bubblepaw_core::TextCallback,
            cancellation: bubblepaw_core::CancellationToken,
        ) -> anyhow::Result<bubblepaw_core::ModelResponse> {
            cancellation.cancelled().await;
            anyhow::bail!("cancelled fixture")
        }
    }

    #[tokio::test]
    async fn running_descendant_blocks_transfer_but_idle_children_do_not() -> anyhow::Result<()> {
        let store = Store::in_memory()?;
        let root = store.create_session("root")?;
        let child =
            store.create_session_with_parent("child", Default::default(), Some(&root.id))?;
        let primary = SessionScope::default();
        let attached = primary.fork();
        primary.adopt_idle_root(&root.id);
        let harness = bubblepaw_core::Harness::new(
            store.clone(),
            Arc::new(PendingProvider),
            Default::default(),
            Default::default(),
        );
        primary.admit_owned(&store, &child.id, || {
            harness.start(&child.id, "wait", Default::default())
        })?;
        assert!(attached.admit(&store, &root.id, true, || Ok(())).is_err());
        assert!(primary.contains(&store, &child.id));
        harness.cancel(&child.id);
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            harness.wait_for_idle(&child.id),
        )
        .await??;
        attached.admit(&store, &root.id, true, || Ok(()))?;
        assert!(attached.contains(&store, &child.id));
        assert!(!primary.contains(&store, &child.id));
        Ok(())
    }
    #[test]
    fn admission_moves_event_ownership_without_claiming_other_roots() {
        let store = Store::in_memory().unwrap();
        let root = store.create_session("root").unwrap();
        let other = store.create_session("other").unwrap();
        let host = SessionScope::default();
        let client = host.fork();
        host.adopt_idle_root(&root.id);
        host.adopt_idle_root(&other.id);
        client.admit(&store, &root.id, true, || Ok(())).unwrap();
        assert!(client.contains(&store, &root.id));
        assert!(!host.contains(&store, &root.id));
        assert!(host.contains(&store, &other.id));
        assert!(!client.contains(&store, &other.id));
        client.release();
        assert!(!client.contains(&store, &root.id));
        assert!(host.contains(&store, &other.id));
    }

    #[test]
    fn completed_request_holds_admission_until_cleanup_and_blocks_child_takeover()
    -> anyhow::Result<()> {
        let store = Store::in_memory()?;
        let root = store.create_session("root")?;
        let child =
            store.create_session_with_parent("child", Default::default(), Some(&root.id))?;
        let first = SessionScope::default();
        let second = first.fork();
        // The synchronous admission can already be complete: the lease lasts
        // until request cleanup, so its waiter/canceller cannot see a successor.
        first.admit(&store, &root.id, true, || Ok(()))?;
        for session in [&root.id, &child.id] {
            let mut admitted = false;
            assert!(
                second
                    .admit(&store, session, true, || {
                        admitted = true;
                        Ok(())
                    })
                    .is_err()
            );
            assert!(!admitted);
        }
        first.release();
        second.admit(&store, &child.id, true, || Ok(()))?;
        assert!(first.admit(&store, &root.id, false, || Ok(())).is_err());
        // Repeated earlier cleanup cannot release the successor's lease.
        first.release();
        assert!(first.admit(&store, &child.id, true, || Ok(())).is_err());
        drop(second.release_on_drop());
        first.admit(&store, &root.id, true, || Ok(()))?;
        Ok(())
    }

    #[test]
    fn failed_and_long_lived_admission_do_not_hold_request_leases() -> anyhow::Result<()> {
        let store = Store::in_memory()?;
        let root = store.create_session("root")?;
        let host = SessionScope::default();
        let client = host.fork();
        host.adopt_idle_root(&root.id);
        assert!(
            client
                .admit(&store, &root.id, true, || -> anyhow::Result<()> {
                    anyhow::bail!("busy core")
                })
                .is_err()
        );
        assert!(host.contains(&store, &root.id));
        host.admit(&store, &root.id, false, || Ok(()))?;
        client.admit(&store, &root.id, true, || Ok(()))?;
        assert!(client.contains(&store, &root.id));
        assert!(!host.contains(&store, &root.id));
        assert!(host.admit_owned(&store, &root.id, || Ok(())).is_err());
        Ok(())
    }
}
