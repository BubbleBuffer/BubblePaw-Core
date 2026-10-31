//! Allocate sessions and their immutable initial chat metadata atomically.
use super::*;

impl Store {
    pub fn create_session(&self, title: impl Into<String>) -> Result<Session> {
        self.create_session_with_strategy(title, ContextStrategy::default())
    }

    pub fn create_session_with_strategy(
        &self,
        title: impl Into<String>,
        context_strategy: ContextStrategy,
    ) -> Result<Session> {
        self.create_session_with_parent(title, context_strategy, None)
    }

    /// Atomically create a root chat with its own immutable canonical workspace.
    pub fn create_session_with_workspace(
        &self,
        title: impl Into<String>,
        workspace: impl AsRef<Path>,
    ) -> Result<Session> {
        self.create_session_with_strategy_and_workspace(
            title,
            ContextStrategy::default(),
            workspace,
        )
    }

    pub fn create_session_with_strategy_and_workspace(
        &self,
        title: impl Into<String>,
        context_strategy: ContextStrategy,
        workspace: impl AsRef<Path>,
    ) -> Result<Session> {
        self.create_session_with_workspace_policy(title, context_strategy, workspace.as_ref(), None)
    }
    pub fn create_session_with_policy(
        &self,
        title: impl Into<String>,
        context_strategy: ContextStrategy,
        workspace: impl AsRef<Path>,
        storage: RootStoragePolicy,
    ) -> Result<Session> {
        self.create_session_with_workspace_policy(
            title,
            context_strategy,
            workspace.as_ref(),
            Some(storage),
        )
    }
    fn create_session_with_workspace_policy(
        &self,
        title: impl Into<String>,
        context_strategy: ContextStrategy,
        workspace: &Path,
        storage: Option<RootStoragePolicy>,
    ) -> Result<Session> {
        let workspace = workspace.canonicalize().context("Workspace must exist")?;
        anyhow::ensure!(workspace.is_dir(), "Workspace must be a directory");
        self.create_session_with_visibility_and_workspace(
            title,
            context_strategy,
            None,
            false,
            Some(&workspace),
            storage,
        )
    }

    pub fn create_session_with_parent(
        &self,
        title: impl Into<String>,
        context_strategy: ContextStrategy,
        parent: Option<&str>,
    ) -> Result<Session> {
        self.create_session_with_visibility(title, context_strategy, parent, false)
    }
    /// Persist host visibility with the initial canonical session, before any run starts.
    pub fn create_session_with_visibility(
        &self,
        title: impl Into<String>,
        context_strategy: ContextStrategy,
        parent: Option<&str>,
        hidden: bool,
    ) -> Result<Session> {
        self.create_session_with_visibility_and_workspace(
            title,
            context_strategy,
            parent,
            hidden,
            None,
            None,
        )
    }
    pub(super) fn create_session_with_visibility_and_workspace(
        &self,
        title: impl Into<String>,
        context_strategy: ContextStrategy,
        parent: Option<&str>,
        hidden: bool,
        workspace: Option<&Path>,
        storage: Option<RootStoragePolicy>,
    ) -> Result<Session> {
        anyhow::ensure!(
            parent.is_none() || (workspace.is_none() && storage.is_none()),
            "Child sessions inherit their root workspace"
        );
        if let Some(storage) = storage {
            storage.validate()?;
        }
        let workspace = if parent.is_none() {
            workspace.map(Path::to_path_buf).or(self.bound_workspace()?)
        } else {
            None
        };
        let parent_hidden = parent
            .map(|id| self.session(id).map(|session| session.hidden))
            .transpose()?
            .unwrap_or(false);
        let hidden = hidden || parent_hidden;
        let execution_mode = self.initial_execution_mode(parent)?;
        if !self.is_layout() {
            let session = Session {
                id: id(),
                title: title.into(),
                parent_session_id: parent.map(str::to_owned),
                parent_cutoff: None,
                created_at: now(),
                updated_at: now(),
                read_only: false,
                hidden,
                context_strategy,
            };
            self.write(|tx| {
                Self::insert_execution_mode(tx, &session.id, &execution_mode)?;
                tx.execute(
                    "INSERT INTO sessions VALUES(?1,?2)",
                    params![session.id, serde_json::to_string(&session)?],
                )?;
                if let Some(workspace) = &workspace {
                    tx.execute(
                        "INSERT INTO metadata VALUES(?1,?2)",
                        params![
                            format!("root_workspace:{}", session.id),
                            workspace.to_string_lossy()
                        ],
                    )?;
                }
                if let Some(storage) = storage {
                    tx.execute(
                        "INSERT INTO metadata VALUES(?1,?2)",
                        params![
                            format!("root_storage:{}", session.id),
                            serde_json::to_string(&storage)?
                        ],
                    )?;
                }
                Ok(())
            })?;
            return Ok(session);
        }
        let session = Session {
            id: id(),
            title: title.into(),
            parent_session_id: parent.map(str::to_owned),
            parent_cutoff: None,
            created_at: now(),
            updated_at: now(),
            read_only: false,
            hidden,
            context_strategy,
        };
        let chat = match parent {
            Some(parent) => self.scoped(parent)?.context("Parent not found")?,
            None => self.bootstrap_chat(&session.id)?,
        };
        chat.write(|tx| {
            Self::insert_execution_mode(tx, &session.id, &execution_mode)?;
            tx.execute(
                "INSERT INTO sessions VALUES(?1,?2)",
                params![session.id, serde_json::to_string(&session)?],
            )?;
            if let Some(workspace) = &workspace {
                tx.execute(
                    "INSERT INTO metadata VALUES(?1,?2)",
                    params![
                        format!("root_workspace:{}", session.id),
                        workspace.to_string_lossy()
                    ],
                )?;
            }
            if let Some(storage) = storage {
                tx.execute(
                    "INSERT INTO metadata VALUES(?1,?2)",
                    params![
                        format!("root_storage:{}", session.id),
                        serde_json::to_string(&storage)?
                    ],
                )?;
            }
            Ok(())
        })?;
        self.write(|tx| {
            tx.execute(
                "INSERT OR REPLACE INTO sessions VALUES(?1,?2)",
                params![session.id, serde_json::to_string(&session)?],
            )?;
            Ok(())
        })?;
        // The buffered serving transaction may hold a stale read snapshot of
        // catalog.db, which cannot upgrade to a write (SQLITE_BUSY_SNAPSHOT). So the
        // buffer is committed first and the catalog row gets its own short
        // transaction, which takes the write lock at BEGIN.
        self.flush_serving()?;
        if parent.is_none() {
            let layout = self.0.layout.as_ref().context("Directory store required")?;
            let mut catalog = layout.catalog.lock().unwrap();
            let tx = catalog.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            tx.execute(
                "INSERT OR REPLACE INTO metadata VALUES(?1,?2)",
                params![
                    format!("root_storage:{}", session.id),
                    serde_json::to_string(&storage)?
                ],
            )?;
            tx.commit()?;
            layout.catalog_transactions.fetch_add(1, Ordering::Relaxed);
        }
        Ok(session)
    }
}
