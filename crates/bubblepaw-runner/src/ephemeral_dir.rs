//! The private directory behind `--no-session` runs and scripts.
//!
//! `tempfile::TempDir` ignores a failed removal. On Windows a file that is
//! still briefly open (a closing SQLite handle, an antivirus scan) makes the
//! removal fail, which would leave the run's spills and store behind. This
//! guard retries for a short while instead.
use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};

const ATTEMPTS: u32 = 50;
const PAUSE: Duration = Duration::from_millis(100);

pub struct EphemeralDir(PathBuf);

impl EphemeralDir {
    pub fn new() -> std::io::Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("bubblepaw-ephemeral-")
            .tempdir()?;
        Ok(Self(directory.keep()))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for EphemeralDir {
    fn drop(&mut self) {
        for attempt in 1..=ATTEMPTS {
            match std::fs::remove_dir_all(&self.0) {
                Ok(()) => return,
                Err(error) if error.kind() == ErrorKind::NotFound => return,
                Err(error) if attempt == ATTEMPTS => {
                    eprintln!(
                        "Could not remove the temporary directory {}: {error}",
                        self.0.display()
                    );
                }
                Err(_) => std::thread::sleep(PAUSE),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_removes_the_directory_and_its_files() {
        let directory = EphemeralDir::new().unwrap();
        let path = directory.path().to_path_buf();
        std::fs::create_dir_all(path.join("data")).unwrap();
        std::fs::write(path.join("data").join("spill.txt"), "spill").unwrap();
        drop(directory);
        assert!(!path.exists());
    }
}
