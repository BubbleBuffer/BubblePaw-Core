//! The requesting side: open one egress session to the paired forwarding
//! device and say which provider host the connection is for.
use super::allow::{self, EGRESS_PORT};
use super::wire::{Hello, Reply};
use crate::remote::adapter::next_message;
use crate::remote::carrier::Carrier;
use crate::remote::client::{RemoteError, device_key, open_session};
use crate::remote::{Limits, RemoteAddress, RemotePaths, SELECT_EGRESS};
use anyhow::{Result, ensure};
use bubblepaw_remote::device::DeviceId;
use bubblepaw_remote::store::{PairStore, PairedHost};
use bubblepaw_remote::tunnel::{self, SplitTunnel};
use std::path::PathBuf;
use std::time::Duration;

/// How long tunnel admission may take.
const ADMIT_WAIT: Duration = Duration::from_secs(30);
/// How long the forwarding device may take to answer a request (it resolves
/// and connects to the provider first).
const REPLY_WAIT: Duration = Duration::from_secs(45);
/// The label of this device's key when it has none yet.
const DEVICE_LABEL: &str = "BubblePaw device";

/// Why model traffic could not go through the forwarding device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EgressError {
    /// The forwarding device's ID.
    pub device_id: String,
    /// Its saved name, or its ID when this device has no saved host for it.
    pub device: String,
    pub kind: EgressErrorKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EgressErrorKind {
    /// This device has no pairing with the forwarding device.
    NotPaired,
    /// The device did not answer: offline, not sharing, or the relay is down.
    Offline(String),
    /// The device answered and said no (`code` is its short reason).
    Refused { code: String, reason: String },
    /// Something on this device failed (for example the local proxy).
    Local(String),
}

impl EgressError {
    fn new(route: &EgressRoute, device: String, kind: EgressErrorKind) -> Self {
        Self {
            device_id: route.device.clone(),
            device,
            kind,
        }
    }

    /// A failure that retrying cannot fix without somebody changing the
    /// pairing or the grant.
    pub fn is_permanent(&self) -> bool {
        match &self.kind {
            EgressErrorKind::NotPaired => true,
            EgressErrorKind::Refused { code, .. } => {
                !matches!(code.as_str(), "unreachable" | "busy")
            }
            EgressErrorKind::Offline(_) | EgressErrorKind::Local(_) => false,
        }
    }
}

impl std::fmt::Display for EgressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let device = if self.device == self.device_id {
            self.device.clone()
        } else {
            format!("\"{}\" ({})", self.device, self.device_id)
        };
        match &self.kind {
            EgressErrorKind::NotPaired => write!(
                f,
                "model traffic cannot go through {device}: this device is not paired with it"
            ),
            EgressErrorKind::Offline(why) => write!(
                f,
                "model traffic through {device} failed: it is offline, not sharing or unreachable ({why})"
            ),
            EgressErrorKind::Refused { reason, .. } => {
                write!(f, "model traffic through {device} was refused: {reason}")
            }
            EgressErrorKind::Local(why) => {
                write!(
                    f,
                    "model traffic through {device} failed on this device: {why}"
                )
            }
        }
    }
}

impl std::error::Error for EgressError {}

/// Where model traffic goes and which hosts it may name.
#[derive(Clone, Debug)]
pub struct EgressRoute {
    /// The forwarding device's ID (a saved host).
    pub device: String,
    /// This device's key file.
    pub key_file: PathBuf,
    /// This device's saved hosts.
    pub hosts: PathBuf,
    pub limits: Limits,
    /// The provider hostnames this route may connect to: the hosts of the
    /// provider config it was made for.
    pub declared: Vec<String>,
}

impl EgressRoute {
    /// A route to `via` for the hosts in `declared`, using the standard files
    /// of `paths`. Fails when `via` is not a device ID or a declared name is
    /// not an allowed public host name.
    pub fn new(paths: &RemotePaths, via: &str, declared: Vec<String>) -> Result<Self> {
        let device = DeviceId::parse(via)
            .map_err(|_| anyhow::anyhow!("`via` must be a saved host's device ID"))?
            .to_string();
        let declared =
            allow::check_declared(&declared).map_err(|refusal| anyhow::anyhow!("{refusal}"))?;
        Ok(Self {
            device,
            key_file: paths.key_file.clone(),
            hosts: paths.hosts.clone(),
            limits: Limits::default(),
            declared,
        })
    }

    /// A route for one provider: its `base_url` must be an `https://` URL
    /// whose host is a public name.
    pub fn for_provider(paths: &RemotePaths, via: &str, base_url: &str) -> Result<Self> {
        let url = reqwest::Url::parse(base_url)
            .map_err(|_| anyhow::anyhow!("the provider's base_url is not a URL"))?;
        ensure!(
            url.scheme() == "https" && url.port().is_none_or(|port| port == EGRESS_PORT),
            "`via` needs an https:// base_url on port 443"
        );
        let host = url
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("the provider's base_url has no host"))?;
        Self::new(paths, via, vec![host.to_owned()])
    }

    /// Open one session for `target` and wait for the device to accept it.
    /// Returns the halves of the admitted tunnel with the byte stream ready.
    pub(super) async fn open(&self, target: &str) -> Result<SplitTunnel<Carrier>, EgressError> {
        let fail = |device: String, kind| EgressError::new(self, device, kind);
        let device_id = DeviceId::parse(&self.device).map_err(|error| {
            fail(
                self.device.clone(),
                EgressErrorKind::Local(error.to_string()),
            )
        })?;
        let hosts = PairStore::<PairedHost>::open(&self.hosts).map_err(|error| {
            fail(
                self.device.clone(),
                EgressErrorKind::Local(format!("could not read the saved hosts: {error}")),
            )
        })?;
        let Some(host) = hosts.get(&device_id).cloned() else {
            return Err(fail(self.device.clone(), EgressErrorKind::NotPaired));
        };
        let device = host.name.clone();
        let local = |why: String| fail(device.clone(), EgressErrorKind::Local(why));
        let offline = |why: String| fail(device.clone(), EgressErrorKind::Offline(why));
        let key =
            device_key(&self.key_file, DEVICE_LABEL).map_err(|error| local(error.to_string()))?;
        let address = RemoteAddress::from_reach(&host.reach_address);
        let session = open_session(
            &address,
            &key,
            &device_id,
            self.limits.clone(),
            SELECT_EGRESS,
        )
        .await
        .map_err(|error| offline(error.to_string()))?;
        let allowed = [host.public_key];
        let tunnel = tokio::time::timeout(ADMIT_WAIT, tunnel::connect(session, &key, &allowed[..]))
            .await
            .map_err(|_| offline("tunnel admission timed out".into()))?
            .map_err(|error| match RemoteError::from(error) {
                RemoteError::Refused => fail(
                    device.clone(),
                    EgressErrorKind::Refused {
                        code: "unpaired".into(),
                        reason:
                            "it does not know this device (it was revoked there, or never paired)"
                                .into(),
                    },
                ),
                other => offline(other.to_string()),
            })?;
        let (mut reader, mut writer) =
            tunnel.split().map_err(|error| offline(error.to_string()))?;
        let hello = serde_json::to_vec(&Hello {
            v: 1,
            target: target.to_owned(),
            hosts: self.declared.clone(),
        })
        .map_err(|error| local(error.to_string()))?;
        writer
            .send(&hello)
            .await
            .map_err(|error| offline(error.to_string()))?;
        let reply = tokio::time::timeout(REPLY_WAIT, next_message(&mut reader))
            .await
            .map_err(|_| offline("it did not answer in time".into()))?
            .ok_or_else(|| offline("it closed the connection".into()))?;
        let reply: Reply = serde_json::from_slice(&reply)
            .map_err(|_| local("the device sent an unreadable answer".into()))?;
        if !reply.ok {
            return Err(fail(
                device,
                EgressErrorKind::Refused {
                    code: reply.code.unwrap_or_default(),
                    reason: reply.reason.unwrap_or_else(|| "no reason given".into()),
                },
            ));
        }
        Ok((reader, writer))
    }
}
