//! Unit tests for chat file ownership and its lease.
use super::*;

#[test]
fn live_clones_share_ownership_and_keep_other_owners_out() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let clone = store.clone();
    let reopened = Store::open_data_dir(directory.path())?;
    assert!(Arc::ptr_eq(&store.0, &reopened.0));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(directory.path().join("store.lock"))?;
    assert!(fs2::FileExt::try_lock_exclusive(&file).is_err());
    drop(store);
    drop(reopened);
    assert!(fs2::FileExt::try_lock_exclusive(&file).is_err());
    drop(clone);
    let _owner = FileLock::acquire(file, "last clone must release ownership")?;
    // A real independent owner still fails immediately; no acquire retry.
    assert!(Store::open_data_dir(directory.path()).is_err());
    Ok(())
}

#[test]
fn reopening_waits_for_the_previous_owner_to_finish_closing() -> Result<()> {
    use std::{sync::mpsc, time::Duration};

    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let session = store.create_session("buffered before close")?;
    let weak = Arc::downgrade(&store.0);
    // Hold the flush boundary so the last Arc expires while close is in progress.
    let connection = store.0.connection.clone();
    let guard = connection.lock().unwrap();
    let result = std::thread::scope(|scope| {
        let closing = scope.spawn(move || drop(store));
        let started = std::time::Instant::now();
        while weak.strong_count() != 0 {
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::yield_now();
        }
        let (send, receive) = mpsc::channel();
        let path = directory.path();
        scope.spawn(move || assert!(send.send(Store::open_data_dir(path)).is_ok()));
        // Before the fix this returns WouldBlock while the old flush is blocked.
        let early = receive.recv_timeout(Duration::from_millis(100));
        drop(guard);
        closing.join().unwrap();
        early.unwrap_or_else(|_| receive.recv_timeout(Duration::from_secs(5)).unwrap())
    });
    drop(connection);
    let reopened = result?;
    assert_eq!(reopened.session(&session.id)?.title, session.title);
    Ok(())
}

#[cfg(unix)]
struct BlockedFork {
    parent: std::os::unix::net::UnixStream,
    spawning: Option<std::thread::JoinHandle<std::io::Result<std::process::ExitStatus>>>,
}
#[cfg(unix)]
impl BlockedFork {
    fn new() -> std::io::Result<Self> {
        use std::{
            io::{Read, Write},
            os::unix::{net::UnixStream, process::CommandExt},
            process::Command,
            time::Duration,
        };

        let (parent, mut child) = UnixStream::pair()?;
        parent.set_read_timeout(Some(Duration::from_secs(5)))?;
        child.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut command = Command::new("true");
        // SAFETY: only reads/writes an existing socket before exec; no
        // allocation, locking or other non-async-signal-safe work occurs.
        unsafe {
            command.pre_exec(move || {
                child.write_all(b"r")?;
                child.read_exact(&mut [0])?;
                Ok(())
            });
        }
        let mut fork = Self {
            parent,
            spawning: Some(std::thread::spawn(move || command.status())),
        };
        fork.parent.read_exact(&mut [0])?;
        Ok(fork)
    }
    fn finish(&mut self) -> std::io::Result<()> {
        use std::io::Write;

        if let Some(spawning) = self.spawning.take() {
            let released = self.parent.write_all(b"c");
            let status = spawning.join().expect("fork thread panicked")?;
            released?;
            assert!(status.success());
        }
        Ok(())
    }
}
#[cfg(unix)]
impl Drop for BlockedFork {
    fn drop(&mut self) {
        // Release and reap our child even when the regression fails.
        let _ = self.finish();
    }
}

#[cfg(unix)]
#[test]
fn closing_unlocks_before_an_unrelated_fork_execs() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let session = store.create_session("inherited lock")?;
    let mut fork = BlockedFork::new()?;
    drop(store);
    let reopened = Store::open_data_dir(directory.path());
    fork.finish()?;
    assert_eq!(reopened?.session(&session.id)?.title, session.title);
    Ok(())
}

#[cfg(unix)]
#[test]
fn export_releases_its_shared_lock_before_an_unrelated_fork_execs() -> Result<()> {
    use std::io::{self, Write};

    struct ForkOnWrite<'a>(&'a mut Option<BlockedFork>);
    impl Write for ForkOnWrite<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.0.is_none() {
                *self.0 = Some(BlockedFork::new()?);
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let session = store.create_session("export shared lock")?;
    store.append_batch(&[Store::message(&session.id, Role::User, "original".into())])?;
    drop(store);
    let mut fork = None;
    assert_eq!(
        Store::export_data_dir_jsonl(directory.path(), &session.id, ForkOnWrite(&mut fork))?,
        1
    );
    let reopened = Store::open_data_dir(directory.path());
    fork.as_mut().unwrap().finish()?;
    assert_eq!(reopened?.messages(&session.id)?[0].content, "original");
    Ok(())
}

struct NoModelCalls;
#[async_trait::async_trait]
impl crate::Provider for NoModelCalls {
    async fn generate(
        &self,
        _: crate::ModelRequest,
        _: crate::TextCallback,
        _: crate::CancellationToken,
    ) -> Result<crate::ModelResponse> {
        bail!("This storage test must not call a model")
    }
}
#[test]
fn rebuild_reconciles_an_interrupted_root_delete_and_invalidates_replay() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (doomed, kept, doomed_path, kept_path) = {
        let store = Store::open_data_dir(directory.path())?;
        let doomed = store.create_session("doomed")?;
        let kept = store.create_session("kept")?;
        store.runtime().log.emit(
            &store,
            &doomed.id,
            "doomed-run",
            crate::EventKind::FollowupQueued {
                text: "synthetic deleted content".into(),
            },
            None,
        );
        store.runtime().log.emit(
            &store,
            &kept.id,
            "kept-run",
            crate::EventKind::FollowupQueued {
                text: "synthetic retained content".into(),
            },
            None,
        );
        let doomed_path = store.chat_path(&doomed.id)?.unwrap();
        let kept_path = store.chat_path(&kept.id)?.unwrap();
        store.idle_checkpoint()?;
        (doomed, kept, doomed_path, kept_path)
    };
    let kept_bytes = std::fs::read(&kept_path)?;
    // Simulate interruption after the authoritative root file was removed,
    // before disposable serving rows/catalog were cleaned.
    std::fs::remove_file(doomed_path)?;
    let store = Store::open_data_dir(directory.path())?;
    let harness = crate::Harness::new(
        store.clone(),
        Arc::new(NoModelCalls),
        crate::ToolRegistry::new(),
        crate::HarnessConfig::default(),
    );
    let events = harness.events_after(0, None, 1000)?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].session_id, kept.id);
    assert_eq!(events[0].sequence, 2);
    assert!(!harness.can_resume_from(0));
    assert!(!harness.can_resume_from(1));
    assert!(harness.can_resume_from(harness.last_sequence()));
    assert!(store.session(&doomed.id).is_err());
    assert_eq!(std::fs::read(&kept_path)?, kept_bytes);
    Ok(())
}
