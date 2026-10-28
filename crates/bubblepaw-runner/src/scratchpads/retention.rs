use super::*;
use std::time::SystemTime;

impl ScratchpadStore {
    /// Remove only seven-day-idle workspaces with no live shared host lease.
    /// The global lock coordinates acquiring leases/creating directories with
    /// this traversal; per-workspace lock files are never removed.
    pub fn sweep_workspace(&self) -> Result<usize> {
        self.sweep_workspace_at(SystemTime::now())
    }

    fn sweep_workspace_at(&self, now: SystemTime) -> Result<usize> {
        let _sweep = self.sweep_lock()?;
        let root = checked_directory(&self.0.data, &["scratch", "workspace"], true)?;
        let locks = checked_directory(&self.0.data, &["scratch", ".workspace-locks"], true)?;
        let mut removed = 0;
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            let identity = entry.file_name();
            let Some(identity) = identity.to_str() else {
                continue;
            };
            if identity.len() != 64 || !identity.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            if !metadata.is_dir() || is_link(&metadata) {
                continue;
            }
            let lock = open_lock(&locks.join(format!("{identity}.lock")))?;
            if fs2::FileExt::try_lock_exclusive(&lock).is_err() {
                continue;
            }
            let path = checked_directory(&self.0.data, &["scratch", "workspace", identity], false)?;
            if idle_tree(&path, now)? {
                fs::remove_dir_all(&path)?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

fn idle_tree(root: &Path, now: SystemTime) -> Result<bool> {
    let mut pending = vec![root.to_owned()];
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path)?;
        if now.duration_since(metadata.modified()?).unwrap_or_default() < WORKSPACE_SCRATCH_MAX_AGE
        {
            return Ok(false);
        }
        // Never follow a symlink or junction out of the scratch tree.
        if metadata.is_dir() && !is_link(&metadata) {
            for entry in fs::read_dir(&path)? {
                pending.push(entry?.path());
            }
        }
    }
    Ok(true)
}
