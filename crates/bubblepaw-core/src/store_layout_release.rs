//! Closing a chat's last handle before its files are deleted. Housekeeping,
//! checkpoints and readers work on clones of a cached chat, and on Windows a
//! SQLite file cannot be removed while any of those keeps it open.
use super::*;
use std::time::{Duration, Instant};

/// How long a delete waits for other users of the chat to finish with it.
const HANDLE_WAIT: Duration = Duration::from_secs(10);
/// Last guard for a handle outside this store, such as a file scanner.
const SHARING_RETRY: Duration = Duration::from_secs(1);

/// Marks a root as being deleted for as long as it lives. While marked, the
/// chat cannot be reopened, so nothing can take a new handle to its files
/// between the eviction and the catalog rows being removed.
pub(super) struct Deleting<'a> {
    layout: &'a Layout,
    root: String,
}

impl<'a> Deleting<'a> {
    /// Evict the chat from the cache and mark it, under the cache lock.
    pub(super) fn evict(layout: &'a Layout, root: &str) -> Self {
        let mut chats = layout.chats.lock().unwrap();
        layout.deleting.lock().unwrap().insert(root.to_owned());
        chats.remove(root);
        Self {
            layout,
            root: root.to_owned(),
        }
    }
}

impl Drop for Deleting<'_> {
    fn drop(&mut self) {
        self.layout.deleting.lock().unwrap().remove(&self.root);
    }
}

/// Refuse to open a chat that a delete is removing. Call it under the chat
/// cache lock.
pub(super) fn ensure_not_deleting(layout: &Layout, root: &str) -> Result<()> {
    anyhow::ensure!(
        !layout.deleting.lock().unwrap().contains(root),
        "Session {root} is being deleted"
    );
    Ok(())
}

/// Wait until `chat` is the only handle to its connection, close it on this
/// thread, then remove the database and its sidecars. The chat must already be
/// out of the layout cache, so no new handle to it can appear.
pub(super) fn remove_chat_files(chat: Store, path: &Path) -> Result<()> {
    let deadline = Instant::now() + HANDLE_WAIT;
    while Arc::strong_count(&chat.0) > 1 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(chat);
    for suffix in ["", "-wal", "-shm"] {
        let file = PathBuf::from(format!("{}{suffix}", path.display()));
        if file.exists() {
            remove_file(&file)?;
        }
    }
    Ok(())
}

fn remove_file(file: &Path) -> std::io::Result<()> {
    let deadline = Instant::now() + SHARING_RETRY;
    loop {
        match std::fs::remove_file(file) {
            // ERROR_SHARING_VIOLATION or ERROR_LOCK_VIOLATION.
            Err(error)
                if cfg!(windows)
                    && matches!(error.raw_os_error(), Some(32 | 33))
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn delete_waits_for_a_housekeeping_handle_to_close() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let store = Store::open_data_dir(directory.path())?;
        let id = store.create_session("held")?.id;
        // A clone of the cached chat, as housekeeping and checkpoints take.
        let held = store.scoped(&id)?.expect("a layout chat");
        let path = held.0.path.clone().expect("a chat file");
        let released = Arc::new(Mutex::new(None));
        let worker = {
            let released = released.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                held.read(|conn| Ok(conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))?))
                    .unwrap();
                *released.lock().unwrap() = Some(Instant::now());
                drop(held);
            })
        };
        store.delete_session(&id)?;
        let finished = Instant::now();
        worker.join().unwrap();
        let released = released.lock().unwrap().expect("the handle was released");
        assert!(
            finished > released,
            "the chat files were removed while another handle kept them open"
        );
        assert!(!path.exists(), "the chat database is removed");
        assert!(store.session(&id).is_err(), "the session is deleted");
        Ok(())
    }

    #[test]
    fn a_chat_being_deleted_cannot_be_reopened() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let store = Store::open_data_dir(directory.path())?;
        let id = store.create_session("reopen")?.id;
        let held = store.scoped(&id)?.expect("a layout chat");
        let layout = store.0.layout.as_ref().unwrap();
        let deleting = Deleting::evict(layout, &id);
        let reopened = store.scoped(&id);
        assert!(reopened.is_err(), "a chat being deleted reopened");
        assert!(!layout.chats.lock().unwrap().contains_key(&id));
        drop(deleting);
        drop(held);
        assert!(
            store.scoped(&id)?.is_some(),
            "the mark ends with the delete"
        );
        Ok(())
    }

    #[test]
    fn delete_without_other_handles_does_not_wait() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let store = Store::open_data_dir(directory.path())?;
        let id = store.create_session("alone")?.id;
        let started = Instant::now();
        store.delete_session(&id)?;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        Ok(())
    }
}
