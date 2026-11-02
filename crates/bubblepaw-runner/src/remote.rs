//! Remote access: a host dials out to a relay or serves its own WebSocket
//! endpoint (published by a Cloudflare Tunnel), paired devices reach it through
//! end-to-end encrypted tunnels, and each tunnel carries the same JSON-RPC
//! frames as stdio, one frame per tunnel message. The tunnel is
//! one more thin transport: the host-API dispatcher enforces the device's
//! profile, reads are host-wide, and nothing here adds methods of its own.
//!
//! The first cleartext frame of every session selects what follows: a
//! host-API tunnel (Noise XX with paired-key admission) or a pairing attempt
//! (XXpsk3 with a one-time code). Either protocol authenticates on its own, so
//! a relay or tunnel that alters the selector can only make the attempt fail.
use anyhow::{Context, Result, bail};
use bubblepaw_remote::device::DeviceId;
pub use bubblepaw_remote::store::DeviceKind;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

mod adapter;
mod carrier;
pub mod cli;
mod cli_tunnel;
mod client;
mod cloudflare_api;
mod cloudflare_status;
mod cloudflared;
mod control;
mod device_key;
mod domain;
mod hosts;
pub mod install;
mod keys;
mod offer;
mod reach;
mod serve_lock;
mod server;
mod server_options;

pub use bubblepaw_remote::relay::Limits;
pub use client::{PairedHostInfo, PendingPairing, RemoteConnect, RemoteError, RemotePair};
pub use cloudflare_status::{
    CloudflareAccount, CloudflareLogin, CloudflareZone, CloudflaredInfo, TokenSource, TunnelLogin,
    cloudflare_account, cloudflare_login_state, cloudflared_info, forget_cloudflare_token,
    store_cloudflare_token, tunnel_login, update_cloudflared,
};
pub use device_key::{
    DeviceKeyChange, DeviceKeyInfo, DeviceKeyPaths, device_key_info, export_device_key,
    import_device_key, regenerate_device_key,
};
pub use hosts::{HostPresence, SavedHost, forget_host, list_hosts, probe_host};
pub use keys::load_or_create_key;
pub use offer::{PairingOffer, PairingRequest};
pub use reach::{CloudflareApi, CloudflareTunnel, RemoteReach, TunnelMode};
pub use serve_lock::{ProcessKind, ServeStatus, serve_status};
pub use server::{RemoteServeOptions, RemoteServer, RemoteStatus, SessionBackend};

pub use client::{connect, pair};
pub use domain::{TOKEN_ENV as CLOUDFLARE_TOKEN_ENV, check_hostname};
pub use install::{CLOUDFLARED_VERSION, InstallSource};

/// The `cloudflared` from `remote tunnel install` in `paths`, else the
/// configured one or `cloudflared` on PATH.
pub fn find_cloudflared(configured: Option<&Path>, paths: &RemotePaths) -> Result<PathBuf> {
    cloudflared::find(configured, &paths.cloudflare_bin())
}

/// Selects a host-API tunnel.
pub(crate) const SELECT_SESSION: &[u8] = b"bubblepaw-remote/host-api/v1";
/// Selects a pairing attempt.
pub(crate) const SELECT_PAIR: &[u8] = b"bubblepaw-remote/pair/v1";
/// An empty tunnel message is a keepalive; both ends send one this often so a
/// quiet session outlives the relay session's idle timeout.
pub(crate) const KEEPALIVE: Duration = Duration::from_secs(20);
/// How long a pairing code stays valid.
pub(crate) const PAIRING_WINDOW: Duration = Duration::from_secs(300);

/// Where a host keeps its remote identity and paired devices.
#[derive(Clone, Debug)]
pub struct RemotePaths {
    /// The host device key (plain `bubblebuffer-device-key` v1, owner-only).
    pub key_file: PathBuf,
    /// Paired devices with their profiles.
    pub clients: PathBuf,
    /// Held by `remote serve` and `remote pair` while they serve the host.
    pub lock: PathBuf,
    /// The serving process's local control endpoint, through which `remote
    /// pair` and `remote devices revoke` reach a running server.
    pub control: PathBuf,
    /// Cloudflare Tunnel state: credentials, the API token file, `cloudflared`.
    pub cloudflare: PathBuf,
    /// This device's saved hosts: the hosts it can open (client side).
    pub hosts: PathBuf,
}

impl RemotePaths {
    /// `<data-dir>/remote/{host.bpkey,clients.json,hosts.json,serve.lock,control/,cloudflare/}`.
    pub fn in_data_dir(data_dir: &Path) -> Self {
        let dir = data_dir.join("remote");
        Self {
            key_file: dir.join("host.bpkey"),
            clients: dir.join("clients.json"),
            lock: dir.join("serve.lock"),
            control: dir.join("control"),
            cloudflare: dir.join("cloudflare"),
            hosts: dir.join("hosts.json"),
        }
    }

    /// Where `remote tunnel install` puts `cloudflared`.
    pub fn cloudflare_bin(&self) -> PathBuf {
        self.cloudflare.join("bin")
    }
}

/// A host-API profile for a paired-device profile; the variants match.
pub(crate) fn api_profile(profile: bubblepaw_remote::store::Profile) -> crate::host_api::Profile {
    use bubblepaw_remote::store::Profile as Paired;
    match profile {
        Paired::Read => crate::host_api::Profile::Read,
        Paired::Edit => crate::host_api::Profile::Edit,
        Paired::Full => crate::host_api::Profile::Full,
    }
}

/// A paired-device profile for a host-API profile.
pub(crate) fn paired_profile(
    profile: crate::host_api::Profile,
) -> bubblepaw_remote::store::Profile {
    use bubblepaw_remote::store::Profile as Paired;
    match profile {
        crate::host_api::Profile::Read => Paired::Read,
        crate::host_api::Profile::Edit => Paired::Edit,
        crate::host_api::Profile::Full => Paired::Full,
    }
}

/// Where a device reaches a host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteAddress {
    /// Through a relay at this `wss://` URL, addressed by the host ID.
    Relay(String),
    /// Straight to the host's own endpoint: a Cloudflare Tunnel address such as
    /// `wss://paw.example.com`, or `ws://127.0.0.1:<port>` on the same machine.
    Direct(String),
}

impl RemoteAddress {
    /// The URL to dial.
    pub fn url(&self) -> &str {
        match self {
            Self::Relay(url) | Self::Direct(url) => url,
        }
    }

    /// The form kept as a paired host's reach address: the relay URL as is,
    /// a direct address as `direct:<url>`.
    pub fn to_reach(&self) -> String {
        match self {
            Self::Relay(url) => url.clone(),
            Self::Direct(url) => format!("direct:{url}"),
        }
    }

    /// Read a paired host's reach address written by [`Self::to_reach`].
    pub fn from_reach(reach: &str) -> Self {
        match reach.strip_prefix("direct:") {
            Some(url) => Self::Direct(url.to_owned()),
            None => Self::Relay(reach.to_owned()),
        }
    }

    pub(crate) fn check(&self) -> Result<()> {
        let url = self.url();
        if !(url.starts_with("wss://") || url.starts_with("ws://")) {
            bail!("a remote address is a ws:// or wss:// URL");
        }
        Ok(())
    }
}

/// What a pairing QR code carries: `{"v":1,"relay":...,"host":...,"code":...}`
/// for a relay host, `{"v":1,"direct":...,"host":...,"code":...}` for a host
/// with its own endpoint. The code is a secret; show it only to the person
/// pairing.
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PairingPayload {
    pub v: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct: Option<String>,
    pub host: String,
    pub code: String,
}

impl std::fmt::Debug for PairingPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingPayload")
            .field("relay", &self.relay)
            .field("direct", &self.direct)
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

impl PairingPayload {
    pub fn new(address: &RemoteAddress, host: String, code: String) -> Self {
        let (relay, direct) = match address {
            RemoteAddress::Relay(url) => (Some(url.clone()), None),
            RemoteAddress::Direct(url) => (None, Some(url.clone())),
        };
        Self {
            v: 1,
            relay,
            direct,
            host,
            code,
        }
    }

    /// Parse and check a scanned payload.
    pub fn parse(text: &str) -> Result<Self> {
        let payload: Self = serde_json::from_str(text.trim()).context("not a pairing code")?;
        if payload.v != 1 {
            bail!("unsupported pairing code version {}", payload.v);
        }
        DeviceId::parse(&payload.host).context("pairing code has an invalid host ID")?;
        let address = match (&payload.relay, &payload.direct) {
            (Some(url), None) => RemoteAddress::Relay(url.clone()),
            (None, Some(url)) => RemoteAddress::Direct(url.clone()),
            _ => bail!("a pairing code names exactly one of relay and direct"),
        };
        address.check()?;
        Ok(payload)
    }

    /// Where the host is reached. A direct address wins over a relay.
    pub fn address(&self) -> RemoteAddress {
        match (&self.relay, &self.direct) {
            (_, Some(url)) => RemoteAddress::Direct(url.clone()),
            (Some(url), None) => RemoteAddress::Relay(url.clone()),
            (None, None) => RemoteAddress::Relay(String::new()),
        }
    }

    /// The compact JSON form for a QR code.
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("payload serializes")
    }
}

#[cfg(test)]
mod payload_tests {
    use super::{PairingPayload, RemoteAddress};

    const HOST: &str = "K7MQ-2X9F-D4RT-8WHN";

    #[test]
    fn a_payload_names_one_address() {
        for address in [
            RemoteAddress::Relay("wss://relay.example".into()),
            RemoteAddress::Direct("wss://abc.trycloudflare.com".into()),
        ] {
            let payload = PairingPayload::new(&address, HOST.into(), "ABCDEFGHJK".into());
            let parsed = PairingPayload::parse(&payload.encode()).unwrap();
            assert_eq!(parsed.address(), address);
            assert_eq!(RemoteAddress::from_reach(&address.to_reach()), address);
        }
        let both =
            format!(r#"{{"v":1,"relay":"wss://a","direct":"wss://b","host":"{HOST}","code":"x"}}"#);
        assert!(PairingPayload::parse(&both).is_err());
        let neither = format!(r#"{{"v":1,"host":"{HOST}","code":"x"}}"#);
        assert!(PairingPayload::parse(&neither).is_err());
        let http = format!(r#"{{"v":1,"direct":"https://b","host":"{HOST}","code":"x"}}"#);
        assert!(PairingPayload::parse(&http).is_err());
    }
}

/// One paired device as the host lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PairedDevice {
    pub id: String,
    pub label: String,
    pub profile: crate::host_api::Profile,
    /// Unix seconds; `None` for entries written before pairing dates were kept.
    pub paired_at: Option<u64>,
    /// What the device declared while pairing; `Other` for older entries.
    pub kind: DeviceKind,
    /// Unix seconds of its last accepted tunnel (written at most once a
    /// minute); `None` if it never connected.
    pub last_seen: Option<u64>,
}

/// The devices in a host's paired-device store, sorted by ID.
pub fn list_devices(clients: &Path) -> Result<Vec<PairedDevice>> {
    let store =
        bubblepaw_remote::store::PairStore::<bubblepaw_remote::store::PairedClient>::open(clients)
            .context("could not read the paired-device store")?;
    Ok(store
        .devices()
        .map(|(id, client)| PairedDevice {
            id: id.to_owned(),
            label: client.name.clone(),
            profile: api_profile(client.profile),
            paired_at: client.paired_at,
            kind: client.kind,
            last_seen: client.last_seen,
        })
        .collect())
}

/// Remove one device from the store. A running server closes its tunnels when
/// it notices the change (within about a second); [`RemoteServer::revoke`]
/// (and `bubblepaw remote devices revoke` while `remote serve` runs) closes
/// them at once. Returns false when no such device was paired.
pub fn revoke_device(clients: &Path, id: &str) -> Result<bool> {
    let id = DeviceId::parse(id).context("invalid device ID")?;
    let mut store =
        bubblepaw_remote::store::PairStore::<bubblepaw_remote::store::PairedClient>::open(clients)
            .context("could not read the paired-device store")?;
    store
        .revoke(&id)
        .context("could not update the paired-device store")
}
