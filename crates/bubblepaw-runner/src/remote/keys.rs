//! Device key files: created on first use, plain (no passphrase), owner-only.
use anyhow::{Context, Result, bail};
use bubblepaw_remote::device::{DeviceKey, KeyFile};
use std::io::{Read, Write};
use std::path::Path;

/// A key file larger than this is not a device key.
const MAX_KEY_FILE: u64 = 64 * 1024;

/// Load the plain device key at `path`, creating it (labelled `label`) when it
/// does not exist yet. A new file is created exclusively with mode 0600 on
/// Unix; on Windows it relies on the directory's ACL. The key is never printed.
pub fn load_or_create_key(path: &Path, label: &str) -> Result<DeviceKey> {
    match std::fs::File::open(path) {
        Ok(file) => read_key(file, path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => create_key(path, label),
        Err(error) => Err(error).with_context(|| format!("could not open {}", path.display())),
    }
}

fn read_key(file: std::fs::File, path: &Path) -> Result<DeviceKey> {
    let mut text = zeroize::Zeroizing::new(String::new());
    file.take(MAX_KEY_FILE + 1)
        .read_to_string(&mut text)
        .with_context(|| format!("could not read {}", path.display()))?;
    if text.len() as u64 > MAX_KEY_FILE {
        bail!("{} is not a device key file", path.display());
    }
    let key_file = KeyFile::parse(&text)
        .with_context(|| format!("{} is not a valid device key file", path.display()))?;
    if key_file.is_protected() {
        bail!(
            "{} is passphrase-protected; BubblePaw needs a plain device key here",
            path.display()
        );
    }
    key_file
        .unlock_device(None)
        .with_context(|| format!("could not load the device key in {}", path.display()))
}

fn create_key(path: &Path, label: &str) -> Result<DeviceKey> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("could not create {}", parent.display()))?;
    let created = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let key_file =
        KeyFile::create(label, &created, None).context("could not create a device key")?;
    let json = zeroize::Zeroizing::new(
        key_file
            .to_json()
            .context("could not encode the device key")?,
    );
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            file.write_all(json.as_bytes())
                .and_then(|()| file.sync_all())
                .with_context(|| format!("could not write {}", path.display()))?;
        }
        // Another process created it first: use that one.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let file = std::fs::File::open(path)
                .with_context(|| format!("could not open {}", path.display()))?;
            return read_key(file, path);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("could not create {}", path.display()));
        }
    }
    key_file
        .unlock_device(None)
        .context("could not load the new device key")
}

#[cfg(test)]
mod tests {
    use super::load_or_create_key;

    #[test]
    fn a_key_is_created_once_and_loaded_after() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remote").join("host.bpkey");
        let created = load_or_create_key(&path, "Test host").unwrap();
        let loaded = load_or_create_key(&path, "ignored").unwrap();
        assert_eq!(created.public_key(), loaded.public_key());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_file_that_is_not_a_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host.bpkey");
        std::fs::write(&path, "{}").unwrap();
        assert!(load_or_create_key(&path, "x").is_err());
    }
}
