//! Caller-located JSON pairing stores. One writer owns each path.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::device::{DeviceId, noise_public_key};
use crate::{Error, Result};

const MAX_STORE_BYTES: u64 = 4 * 1024 * 1024;

/// Host authorization metadata; enforcement belongs to the host dispatcher.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    Read,
    Edit,
    Full,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PairedClient {
    pub public_key: [u8; 32],
    pub name: String,
    pub profile: Profile,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PairedHost {
    pub id: String,
    pub public_key: [u8; 32],
    pub name: String,
    pub reach_address: String,
}

pub trait PeerRecord: Clone + Serialize + DeserializeOwned {
    fn public_key(&self) -> &[u8; 32];
    fn validate(&self) -> Result<()>;

    fn id(&self) -> DeviceId {
        DeviceId::from_public_key(self.public_key())
    }
}

impl PeerRecord for PairedClient {
    fn public_key(&self) -> &[u8; 32] {
        &self.public_key
    }

    fn validate(&self) -> Result<()> {
        validate_name(&self.name)?;
        noise_public_key(&self.public_key)?;
        Ok(())
    }
}

impl PeerRecord for PairedHost {
    fn public_key(&self) -> &[u8; 32] {
        &self.public_key
    }

    fn validate(&self) -> Result<()> {
        validate_name(&self.name)?;
        noise_public_key(&self.public_key)?;
        if DeviceId::parse(&self.id)? != self.id() {
            return Err(Error::Protocol(
                "stored host ID does not match its public key",
            ));
        }
        if self.reach_address.is_empty() || self.reach_address.len() > 2048 {
            return Err(Error::Protocol("invalid host reach address"));
        }
        Ok(())
    }
}

pub(crate) fn validate_name(name: &str) -> Result<()> {
    if name.trim().is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
        return Err(Error::Protocol(
            "device name must contain 1..256 non-control bytes",
        ));
    }
    Ok(())
}

/// An atomic snapshot store. Serialize modifications through a single owner;
/// concurrent writers to the same path would overwrite each other's snapshots.
pub struct PairStore<T> {
    path: PathBuf,
    entries: BTreeMap<String, T>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Document<T> {
    version: u8,
    devices: T,
}

impl<T: PeerRecord> PairStore<T> {
    /// A missing file means an empty store. Corruption is an error, never a reset.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    path,
                    entries: BTreeMap::new(),
                });
            }
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        file.take(MAX_STORE_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_STORE_BYTES {
            return Err(Error::Protocol("pairing store exceeds maximum size"));
        }
        let document: Document<BTreeMap<String, T>> = serde_json::from_slice(&bytes)?;
        if document.version != 1 {
            return Err(Error::Protocol("unsupported pairing store version"));
        }
        for (id, peer) in &document.devices {
            peer.validate()?;
            if id != &peer.id().to_string() {
                return Err(Error::Protocol(
                    "stored device ID does not match its public key",
                ));
            }
        }
        Ok(Self {
            path,
            entries: document.devices,
        })
    }

    pub fn get(&self, id: &DeviceId) -> Option<&T> {
        self.entries.get(&id.to_string())
    }

    pub fn devices(&self) -> impl Iterator<Item = (&str, &T)> {
        self.entries.iter().map(|(id, entry)| (id.as_str(), entry))
    }

    pub fn public_keys(&self) -> Vec<[u8; 32]> {
        self.entries
            .values()
            .map(|peer| *peer.public_key())
            .collect()
    }

    /// Pre-commit failures leave memory unchanged. After rename, memory follows
    /// the new file even if directory sync reports `Error::StoreDurability`.
    pub fn insert(&mut self, peer: T) -> Result<()> {
        peer.validate()?;
        let mut entries = self.entries.clone();
        entries.insert(peer.id().to_string(), peer);
        self.persist(entries)
    }

    pub fn revoke(&mut self, id: &DeviceId) -> Result<bool> {
        let mut entries = self.entries.clone();
        if entries.remove(&id.to_string()).is_none() {
            return Ok(false);
        }
        self.persist(entries)?;
        Ok(true)
    }

    fn persist(&mut self, entries: BTreeMap<String, T>) -> Result<()> {
        self.persist_with_sync(entries, sync_directory)
    }

    fn persist_with_sync(
        &mut self,
        entries: BTreeMap<String, T>,
        sync: impl FnOnce(&Path) -> std::io::Result<()>,
    ) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(&Document {
            version: 1,
            devices: &entries,
        })?;
        if bytes.len() as u64 > MAX_STORE_BYTES {
            return Err(Error::Protocol("pairing store exceeds maximum size"));
        }
        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let mut random = [0u8; 16];
        OsRng
            .try_fill_bytes(&mut random)
            .map_err(|_| Error::Protocol("OS randomness unavailable"))?;
        let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let temp_path = parent.join(format!(".bubblepaw-pairs-{suffix}.tmp"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let result = (|| -> Result<()> {
            let mut file = options.open(&temp_path)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temp_path, &self.path)?;
            self.entries = entries;
            sync(parent).map_err(Error::StoreDurability)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp_path);
        }
        result
    }
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Saved client profiles are the host's admission policy.
impl crate::trust::PeerAuthorizer for PairStore<PairedClient> {
    fn authorize(&self, id: &DeviceId, public_key: &crate::trust::PublicKey) -> Option<Profile> {
        self.get(id)
            .filter(|peer| peer.public_key == *public_key)
            .map(|peer| peer.profile)
    }
}

/// A saved host grants a `Full` connection; request permissions remain a host
/// dispatcher concern. Host records do not contain a per-host profile.
impl crate::trust::PeerAuthorizer for PairStore<PairedHost> {
    fn authorize(&self, id: &DeviceId, public_key: &crate::trust::PublicKey) -> Option<Profile> {
        self.get(id)
            .filter(|peer| peer.public_key == *public_key)
            .map(|_| Profile::Full)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::DeviceKey;

    #[test]
    fn directory_sync_failure_preserves_the_committed_revocation_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clients.json");
        let mut store = PairStore::<PairedClient>::open(&path).unwrap();
        let key = DeviceKey::from_seed(&[19; 32]);
        store
            .insert(PairedClient {
                public_key: key.public_key(),
                name: "Phone".into(),
                profile: Profile::Full,
            })
            .unwrap();
        let result = store.persist_with_sync(BTreeMap::new(), |_| {
            Err(std::io::Error::other("injected directory sync failure"))
        });
        assert!(matches!(result, Err(Error::StoreDurability(_))));
        assert!(
            store.get(&key.id()).is_none(),
            "rename already committed the revocation"
        );
        assert!(
            PairStore::<PairedClient>::open(&path)
                .unwrap()
                .get(&key.id())
                .is_none()
        );
    }
}
