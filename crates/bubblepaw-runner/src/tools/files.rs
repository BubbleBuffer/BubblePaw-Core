use super::{MAX_MUTATION_FILE, WorkspaceTools};
use anyhow::{Result, anyhow, bail, ensure};
use bubblepaw_core::{CancellationToken, Store, ToolContext};
use std::{
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

fn read_mutation_file(path: &Path) -> Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    ensure!(file.metadata()?.is_file(), "path must be a regular file");
    ensure!(
        file.metadata()?.len() <= MAX_MUTATION_FILE,
        "file exceeds 32 MiB limit"
    );
    let mut bytes = Vec::new();
    file.take(MAX_MUTATION_FILE + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_MUTATION_FILE,
        "file exceeds 32 MiB limit"
    );
    Ok(bytes)
}

pub(super) struct FileChange {
    path: PathBuf,
    pub(super) relative: String,
    pub(super) previous: Option<Vec<u8>>,
    pub(super) content: Option<Vec<u8>>,
    permissions: Option<fs::Permissions>,
}
impl FileChange {
    fn error(&self, error: anyhow::Error) -> anyhow::Error {
        anyhow!("file {:?}: {error:#}", self.relative)
    }
}

impl WorkspaceTools {
    /// Restore one checkpoint with the same staging and compare-and-swap checks
    /// used by file writes. The caller holds the workspace mutation guard.
    pub(crate) fn restore_checkpoint(
        &self,
        relative: &str,
        expected_hash: Option<&str>,
        restored: Option<Vec<u8>>,
    ) -> Result<()> {
        self.check_rewind_path(relative)?;
        let path = self.destination(relative)?;
        let previous = if path.exists() {
            Some(read_mutation_file(&path)?)
        } else {
            None
        };
        ensure!(
            crate::rewind::content_hash(previous.as_deref()).as_deref() == expected_hash,
            "file differs from the last checkpoint (outside edit or interrupted write)"
        );
        let permissions = previous
            .as_ref()
            .map(|_| fs::metadata(&path).map(|m| m.permissions()))
            .transpose()?;
        let change = FileChange {
            path,
            relative: relative.to_owned(),
            previous,
            content: restored,
            permissions,
        };
        // A missing file remaining missing needs no filesystem operation.
        if change.previous.is_none() && change.content.is_none() {
            return Ok(());
        }
        self.commit_changes(vec![change], &CancellationToken::new(), || {
            self.check_rewind_path(relative)
        })
    }

    pub(crate) fn checkpoint_current_hash(&self, relative: &str) -> Result<Option<String>> {
        self.check_rewind_path(relative)?;
        let path = self.destination(relative)?;
        if path.exists() {
            Ok(crate::rewind::content_hash(Some(&read_mutation_file(
                &path,
            )?)))
        } else {
            Ok(None)
        }
    }

    fn check_rewind_path(&self, relative: &str) -> Result<()> {
        let mut path = self.root().to_owned();
        for component in Path::new(relative).components() {
            let Component::Normal(component) = component else {
                bail!("checkpoint path must be workspace-relative");
            };
            path.push(component);
            match fs::symlink_metadata(&path) {
                Ok(metadata) => ensure!(
                    !metadata.file_type().is_symlink(),
                    "checkpoint path contains a symlink"
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    pub(super) fn prepare_file_change(&self, relative: &str) -> Result<FileChange> {
        let path = self.destination(relative)?;
        let (previous, permissions) = if path.exists() {
            (
                Some(read_mutation_file(&path)?),
                Some(path.metadata()?.permissions()),
            )
        } else {
            (None, None)
        };
        Ok(FileChange {
            relative: path
                .strip_prefix(&self.root)?
                .iter()
                .map(|component| component.to_string_lossy())
                .collect::<Vec<_>>()
                .join("/"),
            path,
            previous,
            content: None,
            permissions,
        })
    }

    pub(super) fn commit_file_checkpointed(
        &self,
        change: FileChange,
        context: &ToolContext,
        store: &Store,
    ) -> Result<()> {
        ensure!(!context.cancellation.is_cancelled(), "cancelled");
        // Persist the complete intent before replacement, including when the
        // process is interrupted during commit. Large files get rewind barriers.
        crate::rewind::checkpoint_changes(
            store,
            self.root(),
            context,
            std::iter::once((
                change.relative.as_str(),
                change.previous.as_deref(),
                change.content.as_deref(),
            )),
        )?;
        self.commit_changes(vec![change], &context.cancellation, || Ok(()))
    }

    fn unchanged(&self, change: &FileChange) -> Result<()> {
        ensure!(
            self.destination(&change.relative)? == change.path,
            "destination changed during write"
        );
        match &change.previous {
            Some(previous) => ensure!(
                read_mutation_file(&change.path)? == *previous,
                "file changed during write"
            ),
            None => ensure!(!change.path.exists(), "file appeared during write"),
        }
        Ok(())
    }

    fn commit_changes(
        &self,
        changes: Vec<FileChange>,
        cancellation: &CancellationToken,
        before_check: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let mut staged = Vec::with_capacity(changes.len());
        for change in changes {
            ensure!(!cancellation.is_cancelled(), "cancelled");
            let temporary = (|| -> Result<_> {
                let Some(content) = &change.content else {
                    return Ok(None);
                };
                let mut parent = change.path.parent().unwrap();
                while !parent.exists() {
                    parent = parent
                        .parent()
                        .ok_or_else(|| anyhow!("destination has no existing parent"))?;
                }
                let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
                temporary.write_all(content)?;
                if let Some(permissions) = &change.permissions {
                    temporary.as_file().set_permissions(permissions.clone())?;
                }
                temporary.as_file().sync_all()?;
                Ok(Some(temporary))
            })()
            .map_err(|error| change.error(error))?;
            staged.push((change, temporary));
        }
        // Production supplies a no-op. Tests inject an external writer here.
        before_check()?;
        for (change, _) in &staged {
            self.unchanged(change)
                .map_err(|error| change.error(error))?;
        }
        let mut committed: Vec<String> = Vec::new();
        // Create/move destinations before source deletions to preserve a copy if
        // interruption occurs between individual filesystem operations.
        staged.sort_by_key(|(_, temporary)| temporary.is_none());
        for (change, temporary) in staged {
            let result = (|| -> Result<()> {
                ensure!(!cancellation.is_cancelled(), "cancelled");
                self.unchanged(&change)?;
                if let Some(temporary) = temporary {
                    fs::create_dir_all(change.path.parent().unwrap())?;
                    self.unchanged(&change)?;
                    if change.previous.is_none() {
                        temporary
                            .persist_noclobber(&change.path)
                            .map_err(|error| anyhow!(error.error))?;
                    } else {
                        temporary
                            .persist(&change.path)
                            .map_err(|error| anyhow!(error.error))?;
                    }
                } else {
                    fs::remove_file(&change.path)?;
                }
                Ok(())
            })();
            if let Err(error) = result {
                let error = change.error(error);
                if committed.is_empty() {
                    return Err(error);
                }
                bail!(
                    "{error:#}; files partially written; committed paths: {}",
                    serde_json::to_string(&committed)?
                );
            }
            committed.push(change.relative);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_writer_aborts_staged_replacement_and_cleans_temporary_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a.txt");
        fs::write(&path, "original").unwrap();
        let workspace = WorkspaceTools::new(root.path()).unwrap();
        let mut change = workspace.prepare_file_change("a.txt").unwrap();
        change.content = Some(b"tool edit".to_vec());
        let error = workspace
            .commit_changes(vec![change], &CancellationToken::new(), || {
                fs::write(&path, "external writer")?;
                Ok(())
            })
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("a.txt") && error.contains("file changed during write"),
            "{error}"
        );
        assert_eq!(fs::read(&path).unwrap(), b"external writer");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn concurrent_creator_cannot_be_clobbered_by_new_file_write() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a.txt");
        let workspace = WorkspaceTools::new(root.path()).unwrap();
        let mut change = workspace.prepare_file_change("a.txt").unwrap();
        change.content = Some(b"tool content".to_vec());
        let error = workspace
            .commit_changes(vec![change], &CancellationToken::new(), || {
                fs::write(&path, "external creator")?;
                Ok(())
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("file appeared during write"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), b"external creator");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }
}
