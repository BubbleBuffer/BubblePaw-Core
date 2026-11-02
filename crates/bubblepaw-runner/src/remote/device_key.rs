//! This device's key, for an app's "device key" page: show the device ID and
//! fingerprint, export the key to a passphrase-protected file, import one,
//! and regenerate. Import and regenerate change the device ID, so every
//! pairing stops working in both directions: hosts this device paired with no
//! longer know it, and devices paired with this host look for the old host ID.
//! Both stores are cleared and the counts returned. Nothing here logs or
//! returns key material or a passphrase.
use super::RemotePaths;
use super::domain::write_private;
use super::serve_lock::status_at;
use anyhow::{Context, Result, bail};
use bubblepaw_remote::device::{DeviceKey, KeyFile};
use bubblepaw_remote::store::{PairStore, PairedClient, PairedHost};
use serde::Serialize;
use std::io::Read;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// A key file larger than this is not a device key.
const MAX_KEY_FILE: u64 = 64 * 1024;
/// The label a key gets when there is no earlier one to keep.
const DEFAULT_LABEL: &str = "BubblePaw device";

/// Where this device's key and the pairings that depend on it live.
#[derive(Clone, Debug)]
pub struct DeviceKeyPaths {
    /// The device key, plain `bubblebuffer-device-key` v1 (the same file as
    /// `RemotePaths::key_file`, which both host and client roles use).
    pub key_file: PathBuf,
    /// Devices paired with this host; cleared when the key changes.
    pub clients: PathBuf,
    /// Hosts this device can open; cleared when the key changes.
    pub hosts: PathBuf,
    /// The serve lock: while it is held, import and regenerate refuse, because a
    /// running server keeps serving with the old key until it restarts.
    pub lock: Option<PathBuf>,
}

impl DeviceKeyPaths {
    /// The standard locations in `<data_dir>/remote/`.
    pub fn in_data_dir(data_dir: &Path) -> Self {
        Self::from(&RemotePaths::in_data_dir(data_dir))
    }
}

impl From<&RemotePaths> for DeviceKeyPaths {
    fn from(paths: &RemotePaths) -> Self {
        Self {
            key_file: paths.key_file.clone(),
            clients: paths.clients.clone(),
            hosts: paths.hosts.clone(),
            lock: Some(paths.lock.clone()),
        }
    }
}

/// What an app shows for this device's key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DeviceKeyInfo {
    /// The device ID, such as `K7MQ-2X9F-D4RT-8WHN`.
    pub id: String,
    /// SHA-256 of the public key, lowercase hexadecimal.
    pub fingerprint: String,
}

impl DeviceKeyInfo {
    fn of(key: &DeviceKey) -> Self {
        Self {
            id: key.id().to_string(),
            fingerprint: key.fingerprint(),
        }
    }
}

/// What changing the key did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DeviceKeyChange {
    /// The device ID before, if there was a key.
    pub old_id: Option<String>,
    /// The new device ID. Equal to `old_id` when an import brought the same key,
    /// which keeps every pairing.
    pub id: String,
    pub fingerprint: String,
    /// Devices paired with this host that were removed (they cannot reach it
    /// at its new ID).
    pub cleared_devices: usize,
    /// Saved hosts that were removed (they no longer know this device).
    pub cleared_hosts: usize,
}

fn read_key_file(path: &Path) -> Result<KeyFile> {
    let file =
        std::fs::File::open(path).with_context(|| format!("could not open {}", path.display()))?;
    let mut text = Zeroizing::new(String::new());
    file.take(MAX_KEY_FILE + 1)
        .read_to_string(&mut text)
        .with_context(|| format!("could not read {}", path.display()))?;
    if text.len() as u64 > MAX_KEY_FILE {
        bail!("{} is not a device key file", path.display());
    }
    KeyFile::parse(&text)
        .with_context(|| format!("{} is not a valid device key file", path.display()))
}

/// The ID and fingerprint of the device key at `key_file`, creating a plain key
/// (owner-only) the first time. Never returns the private key.
pub fn device_key_info(key_file: &Path) -> Result<DeviceKeyInfo> {
    let key = super::load_or_create_key(key_file, DEFAULT_LABEL)?;
    Ok(DeviceKeyInfo::of(&key))
}

/// Write this device's key to `dest` as a passphrase-protected key file
/// (Argon2id and AES-256-GCM, the BubbleBuffer device-key format). `dest` must
/// not exist. Anyone with the file and the passphrase can act as this device;
/// the passphrase's strength is the caller's policy, but it may not be empty.
pub fn export_device_key(key_file: &Path, dest: &Path, passphrase: &str) -> Result<()> {
    if passphrase.is_empty() {
        bail!("a passphrase is required to export the device key");
    }
    let plain = read_key_file(key_file).context("there is no device key to export yet")?;
    if plain.is_protected() {
        bail!(
            "{} is passphrase-protected; BubblePaw keeps a plain device key here",
            key_file.display()
        );
    }
    let protected = plain
        .rewrap(None, Some(passphrase))
        .context("could not protect the key file")?;
    let json = Zeroizing::new(
        protected
            .to_json()
            .context("could not encode the key file")?,
    );
    if dest.exists() {
        bail!("{} already exists; choose a new file name", dest.display());
    }
    write_private(dest, json.as_bytes())
}

fn refuse_while_serving(paths: &DeviceKeyPaths) -> Result<()> {
    if let Some(lock) = &paths.lock {
        if status_at(lock).shared {
            bail!(
                "this host is being shared right now; stop sharing before changing the device key"
            );
        }
    }
    Ok(())
}

/// Replace the key file with `new` (atomically) and clear both stores unless the
/// ID stayed the same.
fn replace(
    paths: &DeviceKeyPaths,
    old: Option<DeviceKey>,
    new: &KeyFile,
) -> Result<DeviceKeyChange> {
    let device = new
        .unlock_device(None)
        .context("could not load the new device key")?;
    let json = Zeroizing::new(new.to_json().context("could not encode the device key")?);
    let old_id = old.as_ref().map(|key| key.id().to_string());
    let new_id = device.id().to_string();
    if old_id.as_deref() == Some(new_id.as_str()) {
        return Ok(DeviceKeyChange {
            old_id,
            id: new_id,
            fingerprint: device.fingerprint(),
            cleared_devices: 0,
            cleared_hosts: 0,
        });
    }
    write_private(&paths.key_file, json.as_bytes())?;
    let cleared_devices = PairStore::<PairedClient>::open(&paths.clients)
        .context("the device key changed, but the paired-device store could not be read")?
        .clear()
        .context("the device key changed, but the paired-device store could not be cleared")?;
    let cleared_hosts = PairStore::<PairedHost>::open(&paths.hosts)
        .context("the device key changed, but the saved-host store could not be read")?
        .clear()
        .context("the device key changed, but the saved-host store could not be cleared")?;
    Ok(DeviceKeyChange {
        old_id,
        id: new_id,
        fingerprint: device.fingerprint(),
        cleared_devices,
        cleared_hosts,
    })
}

fn current_key(paths: &DeviceKeyPaths) -> Result<Option<DeviceKey>> {
    if !paths.key_file.exists() {
        return Ok(None);
    }
    super::load_or_create_key(&paths.key_file, DEFAULT_LABEL).map(Some)
}

/// Replace this device's key with the one in the key file `src`, unlocked with
/// `passphrase` (`None` for a plain file). A wrong passphrase or a damaged file
/// changes nothing. The key is stored plain in the data directory like any other
/// device key. Importing a key with the same ID keeps every pairing; any other
/// key clears all of them (see [`DeviceKeyChange`]). Refuses while this host is
/// being shared.
pub fn import_device_key(
    paths: &DeviceKeyPaths,
    src: &Path,
    passphrase: Option<&str>,
) -> Result<DeviceKeyChange> {
    refuse_while_serving(paths)?;
    let file = read_key_file(src)?;
    let plain = if file.is_protected() {
        let Some(passphrase) = passphrase.filter(|passphrase| !passphrase.is_empty()) else {
            bail!("the key file is protected; a passphrase is required");
        };
        file.rewrap(Some(passphrase), None)
            .map_err(|_| anyhow::anyhow!("wrong passphrase, or the key file was altered"))?
    } else {
        file
    };
    replace(paths, current_key(paths)?, &plain)
}

/// Replace this device's key with a fresh one. The device ID changes, so every
/// pairing in both directions stops working and both stores are cleared (see
/// [`DeviceKeyChange`]); export the old key first if it may be wanted back.
/// Refuses while this host is being shared.
pub fn regenerate_device_key(paths: &DeviceKeyPaths) -> Result<DeviceKeyChange> {
    refuse_while_serving(paths)?;
    let old = current_key(paths)?;
    let created = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let label = match read_key_file(&paths.key_file) {
        Ok(existing) => existing.label().to_owned(),
        Err(_) => DEFAULT_LABEL.to_owned(),
    };
    let fresh = KeyFile::create(&label, &created, None).context("could not create a device key")?;
    replace(paths, old, &fresh)
}
