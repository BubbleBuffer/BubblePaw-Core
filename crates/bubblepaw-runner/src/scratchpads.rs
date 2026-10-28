//! Disposable files shared by a root chat and by a canonical workspace.
use anyhow::{Context, Result, ensure};
use bubblepaw_core::Store;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

mod retention;
pub const WORKSPACE_SCRATCH_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Clone)]
pub struct ScratchpadStore(Arc<Inner>);
struct Inner {
    data: PathBuf,
    leases: Mutex<BTreeMap<String, WorkspaceScratchLease>>,
    _temporary: Option<tempfile::TempDir>,
}

/// Keep this handle alive for every host using this workspace's scratch files.
/// The shared lock is outside the directory that retention can remove.
#[derive(Clone)]
pub struct WorkspaceScratchLease {
    _lock: Arc<File>,
    pub path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct Scratchpads {
    pub chat: PathBuf,
    pub workspace: PathBuf,
    base: PathBuf,
}

impl ScratchpadStore {
    pub fn new(data_dir: impl AsRef<Path>) -> Result<Self> {
        Self::open(data_dir.as_ref(), None)
    }

    pub(crate) fn temporary() -> Result<Self> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().to_owned();
        Self::open(&path, Some(temporary))
    }

    fn open(data: &Path, temporary: Option<tempfile::TempDir>) -> Result<Self> {
        fs::create_dir_all(data)?;
        let data = data.canonicalize()?;
        checked_directory(&data, &["scratch"], true)?;
        Ok(Self(Arc::new(Inner {
            data,
            leases: Mutex::default(),
            _temporary: temporary,
        })))
    }

    pub fn data_dir(&self) -> &Path {
        &self.0.data
    }

    pub fn for_session(
        &self,
        store: &Store,
        session: &str,
        workspace: impl AsRef<Path>,
    ) -> Result<Scratchpads> {
        let root = store.session_root_id(session)?;
        valid_id(&root)?;
        self.register_cleanup(store);
        let lease = self.lease_workspace(workspace)?;
        let chat = checked_directory(&self.0.data, &["scratch", "chat", &root], true)?;
        Ok(Scratchpads {
            chat,
            workspace: lease.path,
            base: self.0.data.join("scratch"),
        })
    }

    /// Idempotent; root-chat directories use root IDs, so deleting a child does
    /// not remove its root's shared files. No SQLite content is added.
    pub fn register_cleanup(&self, store: &Store) {
        Self::register_cleanup_at(store, &self.0.data);
    }

    /// Register on store open, including delete-only hosts. Keep only the path:
    /// a cleanup callback must not prolong a workspace's live host lease.
    pub fn register_cleanup_at(store: &Store, data: &Path) {
        let data = data.to_owned();
        store.set_session_cleanup(
            format!("scratchpads:{}", data.display()),
            Arc::new(move |session| Self::remove_chat_at(&data, session)),
        );
    }

    fn remove_chat_at(data: &Path, session: &str) -> Result<()> {
        valid_id(session)?;
        let path = data.join("scratch").join("chat").join(session);
        if fs::symlink_metadata(&path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
            return Ok(());
        }
        let path = checked_directory(data, &["scratch", "chat", session], false)?;
        fs::remove_dir_all(path)?;
        Ok(())
    }

    pub fn lease_workspace(&self, workspace: impl AsRef<Path>) -> Result<WorkspaceScratchLease> {
        let canonical = workspace.as_ref().canonicalize()?;
        ensure!(canonical.is_dir(), "workspace must be a directory");
        let identity = workspace_identity(&canonical);
        let mut leases = self
            .0
            .leases
            .lock()
            .map_err(|_| anyhow::anyhow!("scratch leases poisoned"))?;
        if let Some(lease) = leases.get(&identity) {
            checked_directory(&self.0.data, &["scratch", "workspace", &identity], false)?;
            touch(&lease.path)?;
            return Ok(lease.clone());
        }
        let _sweep = self.sweep_lock()?;
        let locks = checked_directory(&self.0.data, &["scratch", ".workspace-locks"], true)?;
        let lock = open_lock(&locks.join(format!("{identity}.lock")))?;
        fs2::FileExt::lock_shared(&lock).context("locking workspace scratch lease")?;
        let path = checked_directory(&self.0.data, &["scratch", "workspace", &identity], true)?;
        touch(&path)?;
        let lease = WorkspaceScratchLease {
            _lock: Arc::new(lock),
            path,
        };
        leases.insert(identity, lease.clone());
        Ok(lease)
    }

    fn sweep_lock(&self) -> Result<File> {
        let root = checked_directory(&self.0.data, &["scratch"], true)?;
        let lock = open_lock(&root.join(".workspace-sweep.lock"))?;
        fs2::FileExt::lock_exclusive(&lock).context("locking workspace scratch sweep")?;
        Ok(lock)
    }
}

impl Scratchpads {
    /// Join a tool's raw path without Windows verbatim-path normalization
    /// erasing `..` before scratch ownership and traversal checks run.
    pub fn resolve_from(
        &self,
        workspace: &Path,
        path: &str,
        allow_missing: bool,
    ) -> Result<Option<PathBuf>> {
        let requested = Path::new(path);
        if requested.is_absolute() {
            return self.resolve(requested, allow_missing);
        }
        #[cfg(windows)]
        if requested.has_root() {
            let prefix = workspace
                .components()
                .next()
                .context("workspace has no drive prefix")?;
            let mut combined = prefix.as_os_str().to_owned();
            combined.push(requested);
            return self.resolve(&PathBuf::from(combined), allow_missing);
        }
        ensure!(
            !requested
                .components()
                .any(|part| matches!(part, Component::Prefix(_))),
            "drive-relative paths are unsupported"
        );
        let mut combined = workspace.as_os_str().to_owned();
        combined.push(std::path::MAIN_SEPARATOR_STR);
        combined.push(requested);
        self.resolve(&PathBuf::from(combined), allow_missing)
    }

    /// Classify an absolute path before trying workspace or read-only grants.
    /// Scratch paths may never traverse `..` or resolve outside their own root,
    /// even when the destination would otherwise be inside the workspace.
    pub fn resolve(&self, path: &Path, allow_missing: bool) -> Result<Option<PathBuf>> {
        let lexical = lexical_suffix(path, &self.base).is_some();
        let resolved = resolve_missing(path, allow_missing);
        let in_scratch = resolved.as_ref().is_ok_and(|p| p.starts_with(&self.base));
        if !lexical && !in_scratch {
            return Ok(None);
        }
        ensure!(
            !path.components().any(|p| p == Component::ParentDir),
            "scratchpad path must not contain .."
        );
        let root = [&self.chat, &self.workspace]
            .into_iter()
            .find(|root| lexical_suffix(path, root).is_some())
            .ok_or_else(|| anyhow::anyhow!("path is outside this chat's scratchpads"))?;
        // Pin the granted identity rather than granting a replaced root's target.
        ensure!(
            root.canonicalize()? == *root,
            "scratchpad root was redirected"
        );
        let resolved = resolved?;
        ensure!(resolved.starts_with(root), "path escapes scratchpad root");
        Ok(Some(resolved))
    }

    pub fn root_for(&self, path: &Path) -> Option<&Path> {
        [&self.chat, &self.workspace]
            .into_iter()
            .find(|root| path.starts_with(root.as_path()))
            .map(PathBuf::as_path)
    }
}

fn workspace_identity(path: &Path) -> String {
    let mut hash = Sha256::new();
    // encode_wide preserves unpaired Windows surrogates; Unix preserves bytes.
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        for unit in path.as_os_str().encode_wide() {
            hash.update(unit.to_le_bytes());
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hash.update(path.as_os_str().as_bytes());
    }
    format!("{:x}", hash.finalize())
}

fn valid_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && Path::new(id).components().count() == 1
            && matches!(
                Path::new(id).components().next(),
                Some(Component::Normal(_))
            ),
        "invalid scratch chat ID"
    );
    Ok(())
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn checked_directory(data: &Path, parts: &[&str], create: bool) -> Result<PathBuf> {
    let mut path = data.to_owned();
    for part in parts {
        path.push(part);
        if create {
            match fs::create_dir(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
        }
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_dir() && !is_link(&metadata),
            "scratch directory must be an ordinary directory"
        );
        ensure!(
            path.canonicalize()? == path,
            "scratch directory was redirected"
        );
    }
    Ok(path)
}

fn open_lock(path: &Path) -> Result<File> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure!(
            metadata.is_file() && !is_link(&metadata),
            "scratch lock must be an ordinary file"
        );
    }
    Ok(File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?)
}

fn touch(path: &Path) -> Result<()> {
    let marker = path.join(".last-used");
    if let Ok(metadata) = fs::symlink_metadata(&marker) {
        ensure!(
            metadata.is_file() && !is_link(&metadata),
            "scratch use marker must be an ordinary file"
        );
    }
    let file = File::options()
        .create(true)
        .truncate(true)
        .write(true)
        .open(marker)?;
    // Truncating an already-empty marker need not change its NTFS timestamp.
    // Last use must remain observable after the host releases its lease.
    file.set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::now()))?;
    Ok(())
}

fn resolve_missing(path: &Path, allow_missing: bool) -> Result<PathBuf> {
    let mut ancestor = path;
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(ancestor) {
            Ok(_) => break,
            Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    ancestor
                        .file_name()
                        .context("scratch path needs a filename")?,
                );
                ancestor = ancestor
                    .parent()
                    .context("scratch path needs an existing ancestor")?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let mut resolved = ancestor.canonicalize()?;
    for part in missing.into_iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

fn lexical_suffix(path: &Path, root: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        // Model paths may omit the verbatim prefix returned by canonicalize.
        // Compare Windows names case-insensitively without resolving `..` or a
        // junction: lexical scratch ownership must survive link redirection.
        fn plain(path: &Path) -> PathBuf {
            let text = path.as_os_str().to_string_lossy();
            if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
                PathBuf::from(format!(r"\\{rest}"))
            } else {
                PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text))
            }
        }
        let path = plain(path);
        let root = plain(root);
        let mut parts = path.components();
        for expected in root.components() {
            let actual = parts.next()?;
            if !actual
                .as_os_str()
                .as_encoded_bytes()
                .eq_ignore_ascii_case(expected.as_os_str().as_encoded_bytes())
            {
                return None;
            }
        }
        Some(parts.as_path().to_owned())
    }
    #[cfg(not(windows))]
    {
        path.strip_prefix(root).ok().map(Path::to_owned)
    }
}
