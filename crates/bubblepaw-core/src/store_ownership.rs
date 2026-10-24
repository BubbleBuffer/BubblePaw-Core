//! Ownership outlives the last strong reference through synchronous teardown.
use super::*;
use std::sync::Condvar;

pub(super) struct Owner {
    pub inner: Weak<Inner>,
    pub released: Arc<Released>,
}

#[derive(Default)]
pub(super) struct Released {
    done: Mutex<bool>,
    changed: Condvar,
}
impl Released {
    pub fn wait(&self) {
        let done = self.done.lock().unwrap();
        drop(self.changed.wait_while(done, |done| !*done).unwrap());
    }
}

pub(super) struct FileLock(std::fs::File);
impl FileLock {
    pub fn acquire(file: std::fs::File, context: &'static str) -> Result<Self> {
        fs2::FileExt::try_lock_exclusive(&file).context(context)?;
        Ok(Self(file))
    }
    pub fn acquire_shared(file: std::fs::File) -> Result<Self> {
        fs2::FileExt::try_lock_shared(&file)?;
        Ok(Self(file))
    }
}
impl Drop for FileLock {
    fn drop(&mut self) {
        // On Unix an unrelated fork can retain this open file description until
        // exec, even with CLOEXEC. Close alone would leave its flock held.
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

pub(super) struct OwnershipLease {
    lock: Option<FileLock>,
    pub released: Arc<Released>,
}
impl OwnershipLease {
    pub fn new(lock: FileLock) -> Self {
        Self {
            lock: Some(lock),
            released: Arc::default(),
        }
    }
}
impl Drop for OwnershipLease {
    fn drop(&mut self) {
        // Inner keeps this lease last: SQLite flush/close precedes unlock, file
        // close precedes notification. Never take OWNERS here: an opener may
        // hold it while waiting for this exact teardown to finish.
        drop(self.lock.take());
        *self.released.done.lock().unwrap() = true;
        self.released.changed.notify_all();
    }
}
