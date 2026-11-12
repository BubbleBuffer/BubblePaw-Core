//! Workspace binding and per-session storage lookups.
use super::*;

impl Store {
    /// Legacy default for chats without an authoritative root workspace.
    pub fn bound_workspace(&self) -> Result<Option<std::path::PathBuf>> {
        let root: Option<String> = if self.is_layout() {
            let layout = self.0.layout.as_ref().unwrap();
            let catalog = layout.catalog.lock().unwrap();
            catalog
                .query_row(
                    "SELECT value FROM metadata WHERE key='workspace'",
                    [],
                    |row| row.get(0),
                )
                .optional()?
        } else {
            let conn = self.0.connection.lock().unwrap();
            conn.query_row(
                "SELECT value FROM metadata WHERE key='workspace'",
                [],
                |row| row.get(0),
            )
            .optional()?
        };
        root.map(|root| Self::workspace_binding(&root)).transpose()
    }
    /// The root chat's workspace, inherited by all of its child sessions.
    /// Reads old bindings without rewriting the chat or restoring an archive.
    pub fn session_workspace(&self, session_id: &str) -> Result<Option<PathBuf>> {
        if self.is_layout() {
            return self.layout_session_workspace(session_id);
        }
        if let Some(host) = self.host() {
            return host.session_workspace(session_id);
        }
        let workspace =
            self.read(|conn| Self::session_workspace_connection(conn, session_id, None))?;
        match workspace {
            Some(workspace) => Ok(Some(workspace)),
            None => self.bound_workspace(),
        }
    }
    /// Physical root-chat identity, including archived chats. This metadata read
    /// never opens a chat file, restores an archive, or rewrites legacy ancestry.
    pub fn session_root_id(&self, session_id: &str) -> Result<String> {
        if let Some(layout) = &self.0.layout {
            if let Some((root, _, _)) = layout.pending_catalog.lock().unwrap().get(session_id) {
                return Ok(root.clone());
            }
            return Ok(layout.catalog.lock().unwrap().query_row(
                "SELECT root_id FROM sessions WHERE id=?1",
                [session_id],
                |row| row.get(0),
            )?);
        }
        if let Some(host) = self.host() {
            return host.session_root_id(session_id);
        }
        self.read(|conn| Self::session_root_connection(conn, session_id, None))
    }
    /// A saved policy is inherited from the physical root chat. None preserves
    /// the host's legacy defaults, without writing policy metadata into old chats.
    pub fn session_storage_policy(&self, session_id: &str) -> Result<Option<RootStoragePolicy>> {
        if self.is_layout() {
            return self.layout_session_storage_policy(session_id);
        }
        if let Some(host) = self.host() {
            return host.session_storage_policy(session_id);
        }
        self.read(|conn| Self::session_storage_connection(conn, session_id, None))
    }
    pub(super) fn session_storage_connection(
        conn: &Connection,
        session_id: &str,
        root_id: Option<&str>,
    ) -> Result<Option<RootStoragePolicy>> {
        let root = Self::session_root_connection(conn, session_id, root_id)?;
        let value: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [format!("root_storage:{root}")],
                |row| row.get(0),
            )
            .optional()?;
        value
            .map(|value| {
                let policy: RootStoragePolicy =
                    serde_json::from_str(&value).context("Malformed root storage policy")?;
                policy.validate()?;
                Ok(policy)
            })
            .transpose()
    }
    pub(super) fn workspace_binding(value: &str) -> Result<PathBuf> {
        let path = PathBuf::from(value);
        // Stored paths remain readable when a chat is moved between Windows
        // and Unix. Execution still requires a directory on the current host.
        let bytes = value.as_bytes();
        let windows_drive = bytes.first().is_some_and(u8::is_ascii_alphabetic)
            && bytes.get(1) == Some(&b':')
            && bytes
                .get(2)
                .is_some_and(|byte| matches!(*byte, b'/' | b'\\'));
        let windows_unc = value.strip_prefix("\\\\").is_some_and(|tail| {
            let mut components = tail.split(['\\', '/']);
            components.next().is_some_and(|server| !server.is_empty())
                && components.next().is_some_and(|share| !share.is_empty())
        });
        let windows_absolute = windows_drive || windows_unc;
        let normalized = if windows_absolute {
            !value
                .split(['\\', '/'])
                .any(|component| matches!(component, "." | ".."))
        } else {
            !path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
        };
        anyhow::ensure!(
            !value.contains('\0')
                && (path.is_absolute() || value.starts_with('/') || windows_absolute)
                && normalized,
            "Malformed workspace binding: expected an absolute normalized path"
        );
        Ok(path)
    }
    pub(super) fn session_workspace_connection(
        conn: &Connection,
        session_id: &str,
        root_id: Option<&str>,
    ) -> Result<Option<PathBuf>> {
        let current = Self::session_root_connection(conn, session_id, root_id)?;
        let workspace: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [format!("root_workspace:{current}")],
                |row| row.get(0),
            )
            .optional()?;
        let workspace = match workspace {
            Some(workspace) => Some(workspace),
            None => conn
                .query_row(
                    "SELECT value FROM metadata WHERE key='workspace'",
                    [],
                    |row| row.get(0),
                )
                .optional()?,
        };
        workspace
            .map(|value| Self::workspace_binding(&value))
            .transpose()
    }
    fn session_root_connection(
        conn: &Connection,
        session_id: &str,
        root_id: Option<&str>,
    ) -> Result<String> {
        let mut current = session_id.to_owned();
        let mut seen = std::collections::HashSet::new();
        loop {
            anyhow::ensure!(
                seen.insert(current.clone()),
                "Session ancestry contains a cycle"
            );
            let data: String =
                conn.query_row("SELECT data FROM sessions WHERE id=?1", [&current], |row| {
                    row.get(0)
                })?;
            let session: Session = serde_json::from_str(&data)?;
            if let Some(root) = root_id {
                // Old migrated forks can have a parent in a different chat.
                // Catalog placement, rather than that historical link, owns
                // their workspace. No source chat needs rewriting.
                current = root.to_owned();
                break;
            }
            match session.parent_session_id {
                Some(parent) => current = parent,
                None => break,
            }
        }
        Ok(current)
    }
    pub fn bind_workspace(&self, path: impl AsRef<Path>) -> Result<()> {
        if self.is_layout() {
            return self.layout_bind_workspace(path.as_ref());
        }
        let root = path.as_ref().canonicalize()?.to_string_lossy().into_owned();
        {
            let conn = self.0.connection.lock().unwrap();
            let mut stmt = conn.prepare("SELECT value FROM metadata WHERE key='workspace'")?;
            let mut rows = stmt.query([])?;
            if let Some(row) = rows.next()? {
                let existing: String = row.get(0)?;
                anyhow::ensure!(
                    existing == root,
                    "Database belongs to another workspace: {existing}"
                );
                return Ok(());
            }
        }
        self.write(|tx| {
            tx.execute(
                "INSERT OR IGNORE INTO metadata VALUES('workspace',?1)",
                [&root],
            )?;
            let existing: String = tx.query_row(
                "SELECT value FROM metadata WHERE key='workspace'",
                [],
                |r| r.get(0),
            )?;
            anyhow::ensure!(
                existing == root,
                "Database belongs to another workspace: {existing}"
            );
            Ok(())
        })
    }
}
