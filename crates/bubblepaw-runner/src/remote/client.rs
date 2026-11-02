//! The device side: pair with a host once, then connect to it through the
//! relay and use it like a local host.
use super::{Limits, SELECT_PAIR, SELECT_SESSION, adapter, load_or_create_key};
use crate::host_client::HostClient;
use bubblepaw_remote::device::{DeviceId, DeviceKey};
use bubblepaw_remote::frame::FrameIo;
use bubblepaw_remote::pairing::{self, ClientOptions, PendingClient};
use bubblepaw_remote::relay::{self, RelaySession};
use bubblepaw_remote::store::{PairStore, PairedHost};
use bubblepaw_remote::tunnel;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long tunnel admission may take.
const ADMIT_WAIT: Duration = Duration::from_secs(30);
/// The label a new device key gets when none is given.
const DEVICE_LABEL: &str = "BubblePaw device";

/// Why pairing with or connecting to a remote host failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteError {
    /// The pairing code was wrong; the host's offer stays open.
    WrongCode,
    /// The pairing code expired, or no offer is open.
    ExpiredCode,
    /// The pairing code was already used.
    ReusedCode,
    /// The person at the host declined.
    Declined,
    /// This device has no pairing with that host.
    NotPaired,
    /// The host does not admit this device (revoked or never paired there).
    Refused,
    /// The relay or the host could not be reached.
    Unreachable(String),
    Other(String),
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongCode => f.write_str("the pairing code is incorrect"),
            Self::ExpiredCode => f.write_str("the pairing code expired"),
            Self::ReusedCode => f.write_str("the pairing code was already used"),
            Self::Declined => f.write_str("the host declined the pairing"),
            Self::NotPaired => f.write_str("this device is not paired with that host"),
            Self::Refused => f.write_str("the remote host refused this device"),
            Self::Unreachable(message) => write!(f, "the remote host is unreachable: {message}"),
            Self::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for RemoteError {}

impl From<bubblepaw_remote::Error> for RemoteError {
    fn from(error: bubblepaw_remote::Error) -> Self {
        use bubblepaw_remote::Error;
        match error {
            Error::WrongCode => Self::WrongCode,
            Error::ExpiredCode => Self::ExpiredCode,
            Error::ReusedCode => Self::ReusedCode,
            Error::ComparisonDeclined => Self::Declined,
            Error::UnpairedPeer => Self::Refused,
            Error::Io(_)
            | Error::WebSocket(_)
            | Error::Closed
            | Error::IdleTimeout
            | Error::AuthenticationFailed
            | Error::Limit(_) => Self::Unreachable(error.to_string()),
            other => Self::Other(other.to_string()),
        }
    }
}

fn other(context: &str, error: impl std::fmt::Display) -> RemoteError {
    RemoteError::Other(format!("{context}: {error}"))
}

/// Where and how to reach a paired remote host.
#[derive(Clone, Debug)]
pub struct RemoteConnect {
    pub relay: String,
    /// The host's device ID.
    pub host_id: String,
    /// This device's key file (created by pairing).
    pub key_file: PathBuf,
    /// This device's paired-host store.
    pub hosts: PathBuf,
    pub limits: Limits,
}

/// What a device needs to pair with a host, usually from its QR code.
#[derive(Clone)]
pub struct RemotePair {
    pub relay: String,
    pub host_id: String,
    /// The one-time code. A secret: never log it.
    pub code: String,
    /// This device's key file; created when missing.
    pub key_file: PathBuf,
    /// This device's paired-host store.
    pub hosts: PathBuf,
    /// The name this device suggests to the host.
    pub device_name: String,
    pub limits: Limits,
}

impl std::fmt::Debug for RemotePair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemotePair")
            .field("relay", &self.relay)
            .field("host_id", &self.host_id)
            .field("device_name", &self.device_name)
            .finish_non_exhaustive()
    }
}

/// The host a device paired with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairedHostInfo {
    pub id: String,
    pub name: String,
    pub relay: String,
}

fn host_id(text: &str) -> Result<DeviceId, RemoteError> {
    DeviceId::parse(text).map_err(|error| other("invalid host ID", error))
}

fn device_key(path: &Path, label: &str) -> Result<DeviceKey, RemoteError> {
    load_or_create_key(path, label).map_err(|error| RemoteError::Other(format!("{error:#}")))
}

async fn open_session(
    relay_url: &str,
    key: &DeviceKey,
    target: &DeviceId,
    limits: Limits,
    selector: &[u8],
) -> Result<RelaySession, RemoteError> {
    let mut session = relay::connect(relay_url, key, target, limits).await?;
    session.send_frame(selector).await?;
    Ok(session)
}

/// Connect to a paired host and return a host-API client for it.
pub async fn connect(options: RemoteConnect) -> Result<HostClient, RemoteError> {
    let target = host_id(&options.host_id)?;
    let hosts = PairStore::<PairedHost>::open(&options.hosts)
        .map_err(|error| other("could not read the paired-host store", error))?;
    let host = hosts.get(&target).ok_or(RemoteError::NotPaired)?.clone();
    let key = device_key(&options.key_file, DEVICE_LABEL)?;
    let session = open_session(
        &options.relay,
        &key,
        &target,
        options.limits,
        SELECT_SESSION,
    )
    .await?;
    let allowed = [host.public_key];
    let tunnel = tokio::time::timeout(ADMIT_WAIT, tunnel::connect(session, &key, &allowed[..]))
        .await
        .map_err(|_| RemoteError::Unreachable("tunnel admission timed out".into()))??;
    let (reader, writer) = tunnel.split()?;
    Ok(adapter::client(reader, writer))
}

/// A pairing in progress on the device: show [`Self::comparison`], then
/// [`Self::finish`] waits for the host's decision (confirm within two
/// minutes, the relay's idle limit).
pub struct PendingPairing {
    pending: PendingClient<RelaySession>,
    hosts: PathBuf,
    relay: String,
}

impl std::fmt::Debug for PendingPairing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingPairing")
            .field("host", &self.pending.host().id)
            .finish_non_exhaustive()
    }
}

impl PendingPairing {
    /// The 6-digit check number the host shows too.
    pub fn comparison(&self) -> &str {
        self.pending.comparison()
    }

    /// The host's name as it introduced itself.
    pub fn host_name(&self) -> &str {
        &self.pending.host().name
    }

    pub fn host_id(&self) -> &str {
        &self.pending.host().id
    }

    /// Wait for the host to approve, then store the host.
    pub async fn finish(self) -> Result<PairedHostInfo, RemoteError> {
        let host = self.pending.host().clone();
        let mut store = PairStore::<PairedHost>::open(&self.hosts)
            .map_err(|error| other("could not read the paired-host store", error))?;
        self.pending.finish(&mut store).await?;
        Ok(PairedHostInfo {
            id: host.id,
            name: host.name,
            relay: self.relay,
        })
    }
}

/// Prove the pairing code to the host. Nothing is stored until
/// [`PendingPairing::finish`] receives the host's approval.
pub async fn pair(options: RemotePair) -> Result<PendingPairing, RemoteError> {
    let target = host_id(&options.host_id)?;
    let key = device_key(&options.key_file, &options.device_name)?;
    let session = open_session(&options.relay, &key, &target, options.limits, SELECT_PAIR).await?;
    let pending = pairing::connect(
        session,
        &key,
        &options.code,
        ClientOptions {
            host_id: target,
            reach_address: options.relay.clone(),
            client_name: options.device_name,
        },
    )
    .await?;
    Ok(PendingPairing {
        pending,
        hosts: options.hosts,
        relay: options.relay,
    })
}
