//! The serve lock: one process at a time shares a data directory's host
//! remotely. `bubblepaw remote serve | pair` and an embedder's
//! `Client::serve_remote` take the same lock (`RemotePaths::lock`), so a
//! second one refuses to start, and [`serve_status`] tells an app whether the
//! host is already being shared and by what.
//!
//! The lock itself is an OS file lock (released when the process dies). Who
//! holds it is written to a sibling `serve.json` after the lock is taken and
//! removed when it is released; the lock file's own bytes stay empty because a
//! Windows lock also blocks other handles from reading them.
use super::RemotePaths;
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// What kind of process holds the serve lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProcessKind {
    /// `bubblepaw remote serve` or `remote pair`.
    Cli,
    /// An application that embeds the host and called `serve_remote`.
    Embedded,
}

/// Whether this data directory's host is being shared remotely, and by whom.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ServeStatus {
    /// Another (or this) process holds the serve lock.
    pub shared: bool,
    /// The holder's process ID, when it said.
    pub pid: Option<u32>,
    /// The holder's kind, when it said.
    pub process: Option<ProcessKind>,
    /// The device ID the holder serves, when it said.
    pub host_id: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Holder {
    pid: u32,
    process: ProcessKind,
    host_id: String,
}

fn info_path(lock: &Path) -> PathBuf {
    lock.with_extension("json")
}

/// The lock, held while a process serves the host remotely. Dropping it
/// releases the lock.
#[derive(Debug)]
pub struct ServeLock {
    _file: std::fs::File,
    info: PathBuf,
}

impl ServeLock {
    /// Take the lock at `lock` and record the holder. Fails when another
    /// process (or this one) already serves this data directory.
    pub fn acquire(lock: &Path, process: ProcessKind, host_id: &str) -> Result<Self> {
        if let Some(parent) = lock.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock)
            .with_context(|| format!("could not open {}", lock.display()))?;
        // A status probe holds the lock for an instant; retry before refusing.
        let mut taken = file.try_lock_exclusive().is_ok();
        for _ in 0..3 {
            if taken {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
            taken = file.try_lock_exclusive().is_ok();
        }
        if !taken {
            let by = match read_holder(lock) {
                Some(holder) => format!(" (process {})", holder.pid),
                None => String::new(),
            };
            bail!(
                "another `bubblepaw remote serve`, `remote pair` or embedded host{by} is serving \
                 this data directory and its control endpoint did not answer; stop it first"
            );
        }
        let holder = Holder {
            pid: std::process::id(),
            process,
            host_id: host_id.to_owned(),
        };
        let info = info_path(lock);
        // Best effort: the lock is what counts; the note only says who holds it.
        if let Err(error) = super::domain::write_private(&info, &serde_json::to_vec(&holder)?) {
            tracing::debug!("could not record the serve lock holder: {error:#}");
        }
        Ok(Self { _file: file, info })
    }
}

impl Drop for ServeLock {
    fn drop(&mut self) {
        // Remove the note before the lock goes with the file handle.
        let _ = std::fs::remove_file(&self.info);
    }
}

fn read_holder(lock: &Path) -> Option<Holder> {
    let bytes = std::fs::read(info_path(lock)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Whether the host of `paths`' data directory is being shared by a process,
/// and which one. A probe, not a reservation: the answer can change at once.
pub fn serve_status(paths: &RemotePaths) -> ServeStatus {
    status_at(&paths.lock)
}

pub(super) fn status_at(lock: &Path) -> ServeStatus {
    let free = ServeStatus {
        shared: false,
        pid: None,
        process: None,
        host_id: None,
    };
    let Ok(file) = std::fs::File::open(lock) else {
        return free;
    };
    if FileExt::try_lock_shared(&file).is_ok() {
        let _ = FileExt::unlock(&file);
        return free;
    }
    let holder = read_holder(lock);
    ServeStatus {
        shared: true,
        pid: holder.as_ref().map(|holder| holder.pid),
        process: holder.as_ref().map(|holder| holder.process),
        host_id: holder.map(|holder| holder.host_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_is_exclusive_and_status_names_the_holder() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RemotePaths::in_data_dir(dir.path());
        assert!(!serve_status(&paths).shared);
        let lock =
            ServeLock::acquire(&paths.lock, ProcessKind::Embedded, "K7MQ-2X9F-D4RT-8WHN").unwrap();
        let status = serve_status(&paths);
        assert!(status.shared);
        assert_eq!(status.pid, Some(std::process::id()));
        assert_eq!(status.process, Some(ProcessKind::Embedded));
        assert_eq!(status.host_id.as_deref(), Some("K7MQ-2X9F-D4RT-8WHN"));
        let second = ServeLock::acquire(&paths.lock, ProcessKind::Cli, "X").unwrap_err();
        assert!(
            second
                .to_string()
                .contains("is serving this data directory")
        );
        drop(lock);
        assert!(!serve_status(&paths).shared);
        ServeLock::acquire(&paths.lock, ProcessKind::Cli, "X").unwrap();
    }
}
