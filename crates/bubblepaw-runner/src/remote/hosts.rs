//! The device side's saved hosts: the hosts this device paired with and can
//! open, plus a cheap online probe for each. The store is the
//! `PairStore<PairedHost>` that `remote::pair` and `connect_remote` already
//! use (`RemotePaths::hosts` by default).
use super::client::{device_key, other};
use super::{Limits, RemoteAddress, RemoteError, api_profile};
use anyhow::{Context, Result};
use bubblepaw_remote::device::DeviceId;
use bubblepaw_remote::store::{PairStore, PairedHost};
use bubblepaw_remote::{direct, relay};
use serde::Serialize;
use std::path::Path;
use std::time::Duration;

/// Label of the key a probe loads when this device has none yet (a device
/// that saved a host always has one).
const PROBE_LABEL: &str = "BubblePaw device";

/// One host this device can open.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SavedHost {
    /// The host's device ID.
    pub id: String,
    /// The name the host gave itself while pairing.
    pub label: String,
    /// Where it is reached (pass it to `RemoteConnect`).
    pub address: RemoteAddress,
    /// The profile the host granted this device; `None` for hosts saved before
    /// the grant was kept.
    pub profile: Option<crate::host_api::Profile>,
    /// Whether the host lets this device send model traffic through it.
    pub egress: bool,
    /// Unix seconds when the host approved the pairing.
    pub paired_at: Option<u64>,
    /// Unix seconds of the last successful `connect_remote`.
    pub last_connected: Option<u64>,
}

/// Whether a saved host answers now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HostPresence {
    Online,
    Offline,
}

impl From<&PairedHost> for SavedHost {
    fn from(host: &PairedHost) -> Self {
        Self {
            id: host.id.clone(),
            label: host.name.clone(),
            address: RemoteAddress::from_reach(&host.reach_address),
            profile: host.profile.map(api_profile),
            egress: host.egress,
            paired_at: host.paired_at,
            last_connected: host.last_connected,
        }
    }
}

/// The hosts in the saved-host store, sorted by ID.
pub fn list_hosts(hosts: &Path) -> Result<Vec<SavedHost>> {
    let store =
        PairStore::<PairedHost>::open(hosts).context("could not read the saved-host store")?;
    Ok(store.devices().map(|(_, host)| host.into()).collect())
}

/// Forget a saved host. Only this device's pairing goes: the host keeps this
/// device in its own list until it is revoked there. Returns false when no
/// such host was saved.
pub fn forget_host(hosts: &Path, id: &str) -> Result<bool> {
    let id = DeviceId::parse(id).context("invalid host ID")?;
    let mut store =
        PairStore::<PairedHost>::open(hosts).context("could not read the saved-host store")?;
    store
        .revoke(&id)
        .context("could not update the saved-host store")
}

/// Ask whether a saved host is online, giving up after `timeout`.
///
/// For a relay host the relay is asked (a signed presence query that reveals
/// only online or offline for this one ID, and opens no session on the host);
/// an unreachable relay is an error, because then nothing is known. For a
/// direct or Cloudflare address the probe is a WebSocket connection attempt
/// that is dropped at once; any failure to connect reads as offline.
pub async fn probe_host(
    host: &SavedHost,
    key_file: &Path,
    limits: Limits,
    timeout: Duration,
) -> Result<HostPresence, RemoteError> {
    match &host.address {
        RemoteAddress::Direct(url) => Ok(match direct::connect(url, timeout).await {
            Ok(socket) => {
                drop(socket);
                HostPresence::Online
            }
            Err(_) => HostPresence::Offline,
        }),
        RemoteAddress::Relay(url) => {
            let target =
                DeviceId::parse(&host.id).map_err(|error| other("invalid host ID", error))?;
            let key = device_key(key_file, PROBE_LABEL)?;
            let asked = tokio::time::timeout(timeout, relay::probe(url, &key, &target, limits))
                .await
                .map_err(|_| RemoteError::Unreachable("the relay did not answer in time".into()))?;
            Ok(if asked? {
                HostPresence::Online
            } else {
                HostPresence::Offline
            })
        }
    }
}
