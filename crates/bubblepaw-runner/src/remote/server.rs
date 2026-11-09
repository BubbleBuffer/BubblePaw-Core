//! The host side: reachable through a relay or its own endpoint, it admits
//! paired devices into tunnels and serves each through the host API with the
//! device's profile.
pub use super::RemoteStatus;
use super::egress::{EgressDial, SystemDial};
use super::offer::{self, OfferSlot, PairingOffer};
use super::reach::{self, RemoteReach};
use super::serve_lock::ServeLock;
pub use super::server_options::{RemoteServeOptions, SessionBackend};
use super::{
    PairedDevice, RemoteAddress, SELECT_EGRESS, SELECT_PAIR, SELECT_SESSION, adapter, api_profile,
    cloudflared, control, list_devices, load_or_create_key, revoke_device,
};
use super::{ShareFailure, ShareState, ShareStateHandle};
use crate::host_api::Profile;
use anyhow::{Context, Result, anyhow};
use bubblepaw_core::CancellationToken;
use bubblepaw_remote::device::{DeviceId, DeviceKey};
use bubblepaw_remote::frame::SplitFrames;
use bubblepaw_remote::relay;
use bubblepaw_remote::store::{PairStore, PairedClient};
use bubblepaw_remote::trust::DeviceListAuthorizer;
use bubblepaw_remote::tunnel;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// How long a new session may take to say what it wants.
#[path = "server_sessions.rs"]
mod sessions;

const SELECT_WAIT: Duration = Duration::from_secs(10);
/// How long tunnel admission may take.
const ADMIT_WAIT: Duration = Duration::from_secs(30);
/// A device's `last_seen` is rewritten at most this often.
const SEEN_EVERY: Duration = Duration::from_secs(60);
/// How often the paired-device store is checked for changes by other processes.
const STORE_POLL: Duration = Duration::from_secs(1);
/// Egress sessions served at once; more are refused as busy.
const EGRESS_MAX: usize = 64;
pub(super) const BACKOFF_MIN: Duration = Duration::from_secs(1);
pub(super) const BACKOFF_MAX: Duration = Duration::from_secs(60);

struct Live {
    device: String,
    profile: Profile,
    /// A forwarding session that holds the egress grant: it ends when the
    /// device loses the grant, whatever its profile does. Other sessions (a
    /// host-API tunnel, or an egress request about to be refused) end when the
    /// device is revoked or its profile changes.
    egress: bool,
    stop: CancellationToken,
}

pub(super) struct Inner {
    options: RemoteServeOptions,
    key: DeviceKey,
    backend: Option<SessionBackend>,
    authorizer: DeviceListAuthorizer,
    stamp: Mutex<Option<(SystemTime, u64)>>,
    tunnels: Mutex<HashMap<u64, Live>>,
    /// Devices holding the egress grant, as of the last store read.
    egress_ids: Mutex<HashSet<String>>,
    egress_dial: Arc<dyn EgressDial>,
    egress_slots: Arc<tokio::sync::Semaphore>,
    next: AtomicU64,
    offer: OfferSlot,
    store_writes: tokio::sync::Mutex<()>,
    pub(super) status: watch::Sender<RemoteStatus>,
    pub(super) address: watch::Sender<Option<RemoteAddress>>,
    pub(super) last_error: Mutex<Option<String>>,
    pub(super) failure: Mutex<Option<ShareFailure>>,
    pub(super) stop: CancellationToken,
}

/// A running remote server. Dropping it stops the server, closes its tunnels
/// and stops its `cloudflared`.
pub struct RemoteServer {
    inner: Arc<Inner>,
    port: Option<u16>,
    tasks: Vec<JoinHandle<()>>,
    _lock: Option<ServeLock>,
}

impl RemoteServer {
    /// Load or create the host key, read the paired devices and start making
    /// the host reachable. With no backend, the server only pairs and refuses
    /// tunnels. Call from within a Tokio runtime.
    pub fn start(options: RemoteServeOptions, backend: Option<SessionBackend>) -> Result<Self> {
        Self::start_with_dial(options, backend, Arc::new(SystemDial))
    }

    /// [`Self::start`] with a custom way to reach provider hosts for devices
    /// granted egress. The default ([`SystemDial`]) resolves public names only;
    /// this exists so tests can reach a local fake provider.
    #[doc(hidden)]
    pub fn start_with_dial(
        options: RemoteServeOptions,
        backend: Option<SessionBackend>,
        egress_dial: Arc<dyn EgressDial>,
    ) -> Result<Self> {
        let key = load_or_create_key(&options.key_file, &options.host_name)?;
        let lock = match &options.lock {
            Some(path) => Some(ServeLock::acquire(
                path,
                options.process,
                &key.id().to_string(),
            )?),
            None => None,
        };
        let inner = Arc::new(Inner {
            authorizer: DeviceListAuthorizer::new(std::iter::empty())
                .map_err(|error| anyhow!("{error}"))?,
            options,
            key,
            backend,
            stamp: Mutex::new(None),
            tunnels: Mutex::new(HashMap::new()),
            egress_ids: Mutex::new(HashSet::new()),
            egress_dial,
            egress_slots: Arc::new(tokio::sync::Semaphore::new(EGRESS_MAX)),
            next: AtomicU64::new(1),
            offer: OfferSlot::default(),
            store_writes: tokio::sync::Mutex::new(()),
            status: watch::Sender::new(RemoteStatus::Connecting),
            address: watch::Sender::new(None),
            last_error: Mutex::new(None),
            failure: Mutex::new(None),
            stop: CancellationToken::new(),
        });
        if let Some(SessionBackend::Context(ctx)) = &inner.backend {
            *ctx.remote_share.lock().unwrap() = Some(ShareStateHandle(Arc::downgrade(&inner)));
        }
        inner.refresh(true)?;
        let mut tasks = vec![tokio::spawn(poll_store(inner.clone()))];
        let mut port = None;
        match inner.options.reach.clone() {
            RemoteReach::Relay(url) => {
                inner.address.send_replace(Some(RemoteAddress::Relay(url)));
                tasks.push(tokio::spawn(relay_loop(inner.clone())));
            }
            RemoteReach::Local {
                port: wanted,
                address,
            } => {
                let listener = reach::bind(wanted)?;
                let bound = listener.local_addr()?.port();
                port = Some(bound);
                let address = address.unwrap_or_else(|| format!("ws://127.0.0.1:{bound}"));
                inner
                    .address
                    .send_replace(Some(RemoteAddress::Direct(address)));
                inner.status.send_replace(RemoteStatus::Online);
                tasks.push(tokio::spawn(reach::direct_loop(inner.clone(), listener)));
            }
            RemoteReach::Cloudflare(tunnel) => {
                let listener = reach::bind(tunnel.port)?;
                let bound = listener.local_addr()?.port();
                port = Some(bound);
                tasks.push(tokio::spawn(reach::direct_loop(inner.clone(), listener)));
                tasks.push(tokio::spawn(cloudflared::supervise(
                    inner.clone(),
                    tunnel,
                    bound,
                )));
            }
        }
        Ok(Self {
            inner,
            port,
            tasks,
            _lock: lock,
        })
    }

    /// This host's device ID, which devices need to reach it.
    pub fn host_id(&self) -> String {
        self.inner.key.id().to_string()
    }

    pub fn status(&self) -> RemoteStatus {
        *self.inner.status.borrow()
    }

    /// A receiver that sees every status change.
    pub fn watch_status(&self) -> watch::Receiver<RemoteStatus> {
        self.inner.status.subscribe()
    }

    /// Where devices reach the host now; `None` until a quick tunnel has its
    /// address. A quick tunnel's address changes when `cloudflared` restarts.
    pub fn address(&self) -> Option<RemoteAddress> {
        self.inner.address.borrow().clone()
    }

    /// A receiver that sees every address change.
    pub fn watch_address(&self) -> watch::Receiver<Option<RemoteAddress>> {
        self.inner.address.subscribe()
    }

    /// The local port of the host's own endpoint (not for relay reach).
    pub fn local_port(&self) -> Option<u16> {
        self.port
    }

    /// Why the host last went offline, if it did.
    pub fn last_error(&self) -> Option<String> {
        self.inner.last_error.lock().unwrap().clone()
    }

    pub fn share_state(&self) -> ShareState {
        self.inner.share_state()
    }

    /// Wait until devices can reach the host.
    pub async fn wait_online(&self, timeout: Duration) -> Result<()> {
        let mut status = self.inner.status.subscribe();
        tokio::time::timeout(
            timeout,
            status.wait_for(|status| *status == RemoteStatus::Online),
        )
        .await
        .map_err(|_| match self.last_error() {
            Some(error) => anyhow!("the host did not come online in time: {error}"),
            None => anyhow!("the host did not come online in time"),
        })?
        .map_err(|_| anyhow!("the remote server stopped"))?;
        Ok(())
    }

    /// Open a pairing offer for a device that will get `profile`. Replaces an
    /// earlier offer. The offer carries the current address, so the host must
    /// have one (see [`Self::wait_online`]).
    pub async fn pair(&self, profile: Profile) -> Result<PairingOffer> {
        self.inner.pair(profile, false).await
    }

    /// Like [`Self::pair`], and `egress` also lets the device send its model
    /// traffic through this host (see [`egress`]). The grant is separate from
    /// the profile: a `read` device can hold it, a `full` device need not.
    pub async fn pair_with_egress(&self, profile: Profile, egress: bool) -> Result<PairingOffer> {
        self.inner.pair(profile, egress).await
    }

    pub fn devices(&self) -> Result<Vec<PairedDevice>> {
        list_devices(&self.inner.options.clients)
    }

    /// Remove a device and close its live tunnels now. Returns false when no
    /// such device was paired.
    pub async fn revoke(&self, id: &str) -> Result<bool> {
        self.inner.revoke(id).await
    }

    /// The number of open tunnels.
    pub fn tunnel_count(&self) -> usize {
        self.inner.tunnels.lock().unwrap().len()
    }

    /// Serve a local control endpoint in `dir` (the current user only, like
    /// the host's own control endpoint) so other processes can pair and
    /// revoke through this server: `bubblepaw remote pair` and `remote
    /// devices revoke` use `RemotePaths::control`.
    pub async fn open_control(&mut self, dir: &Path) -> Result<()> {
        // The Unix endpoint creates its directory; the Windows pipe does not.
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let endpoint = crate::host_transport::HostEndpoint::bind(dir)
            .await
            .context("could not open the remote control endpoint")?;
        self.tasks
            .push(tokio::spawn(control::serve(self.inner.clone(), endpoint)));
        Ok(())
    }

    /// Stop serving, close every tunnel, stop `cloudflared` and wait for the
    /// server's tasks.
    pub async fn shutdown(mut self) {
        self.inner.stop.cancel();
        for task in std::mem::take(&mut self.tasks) {
            let _ = task.await;
        }
    }
}

impl Drop for RemoteServer {
    fn drop(&mut self) {
        self.inner.stop.cancel();
    }
}

impl Inner {
    pub(super) async fn pair(&self, profile: Profile, egress: bool) -> Result<PairingOffer> {
        let address = self
            .address
            .borrow()
            .clone()
            .context("the host has no public address yet; wait until it is online")?;
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        PairingOffer::install(&self.offer, id, &address, &self.key.id(), profile, egress).await
    }

    pub(super) async fn revoke(&self, id: &str) -> Result<bool> {
        let removed = {
            let _writing = self.store_writes.lock().await;
            revoke_device(&self.options.clients, id)?
        };
        self.refresh(true)?;
        Ok(removed)
    }

    /// Record that a device opened a tunnel, at most once a minute per device,
    /// so a busy phone does not rewrite the store on every reconnect. A failure
    /// to write never refuses the device.
    async fn touch_last_seen(&self, id: &DeviceId) {
        let _writing = self.store_writes.lock().await;
        let now = bubblepaw_remote::store::unix_now();
        let result = (|| -> Result<()> {
            let mut store = PairStore::<PairedClient>::open(&self.options.clients)?;
            let recent = store.get(id).is_some_and(|client| {
                client
                    .last_seen
                    .zip(now)
                    .is_some_and(|(seen, now)| now.saturating_sub(seen) < SEEN_EVERY.as_secs())
            });
            if !recent {
                store.update(id, |client| client.last_seen = now)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            tracing::debug!("could not record when a device was last seen: {error:#}");
        }
    }

    pub(super) fn set_offline(&self, error: Option<String>) {
        if let Some(error) = &error {
            tracing::warn!("{error}");
        }
        *self.last_error.lock().unwrap() = error;
        self.status.send_replace(RemoteStatus::Offline);
    }

    /// Reload the paired devices when the store changed (or always with
    /// `force`): new admissions use the new list, and live tunnels whose
    /// device is gone or whose profile changed are closed.
    fn refresh(&self, force: bool) -> Result<()> {
        let stamp = std::fs::metadata(&self.options.clients).ok().map(|meta| {
            (
                meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                meta.len(),
            )
        });
        {
            let mut known = self.stamp.lock().unwrap();
            if !force && *known == stamp {
                return Ok(());
            }
            *known = stamp;
        }
        let store = PairStore::<PairedClient>::open(&self.options.clients)
            .map_err(|error| anyhow!("could not read the paired-device store: {error}"))?;
        let devices: HashMap<String, Profile> = store
            .devices()
            .map(|(id, client)| (id.to_owned(), api_profile(client.profile)))
            .collect();
        let egress_ids: HashSet<String> = store
            .devices()
            .filter(|(_, client)| client.egress)
            .map(|(id, _)| id.to_owned())
            .collect();
        *self.egress_ids.lock().unwrap() = egress_ids.clone();
        self.authorizer
            .replace(store.devices().map(|(_, client)| {
                (
                    DeviceId::from_public_key(&client.public_key),
                    client.public_key,
                    client.profile,
                )
            }))
            .map_err(|error| anyhow!("invalid paired-device store: {error}"))?;
        for live in self.tunnels.lock().unwrap().values() {
            let current = if live.egress {
                egress_ids.contains(&live.device)
            } else {
                devices.get(&live.device) == Some(&live.profile)
            };
            if !current {
                live.stop.cancel();
            }
        }
        Ok(())
    }
}

async fn relay_loop(inner: Arc<Inner>) {
    let RemoteReach::Relay(url) = inner.options.reach.clone() else {
        return;
    };
    let mut backoff = BACKOFF_MIN;
    loop {
        inner.status.send_replace(RemoteStatus::Connecting);
        let registered = tokio::select! {
            _ = inner.stop.cancelled() => return,
            host = relay::register(&url, &inner.key, inner.options.limits.clone()) => host,
        };
        let error = match registered {
            Ok(mut host) => {
                backoff = BACKOFF_MIN;
                *inner.last_error.lock().unwrap() = None;
                *inner.failure.lock().unwrap() = None;
                inner.status.send_replace(RemoteStatus::Online);
                loop {
                    let session = tokio::select! {
                        _ = inner.stop.cancelled() => return,
                        session = host.accept() => session,
                    };
                    match session {
                        Ok(session) => {
                            tokio::spawn(handle(inner.clone(), session));
                        }
                        Err(error) => break format!("relay registration ended: {error}"),
                    }
                }
            }
            Err(error) => format!("could not register at the relay: {error}"),
        };
        *inner.failure.lock().unwrap() = Some(ShareFailure {
            transport: "relay".into(),
            exit_code: None,
            retry_after_secs: backoff.as_secs(),
        });
        inner.set_offline(Some(error));
        tokio::select! {
            _ = inner.stop.cancelled() => return,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

async fn poll_store(inner: Arc<Inner>) {
    let mut interval = tokio::time::interval(STORE_POLL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = inner.stop.cancelled() => break,
            _ = interval.tick() => {
                if let Err(error) = inner.refresh(false) {
                    tracing::warn!("{error:#}");
                }
            }
        }
    }
    // Server stopped: close what is still open.
    for live in inner.tunnels.lock().unwrap().values() {
        live.stop.cancel();
    }
}

/// Serve one session (a relay session or a direct socket): its first frame
/// selects a host-API tunnel or a pairing attempt.
pub(super) async fn handle<T: SplitFrames + 'static>(inner: Arc<Inner>, mut session: T) {
    let selector = tokio::select! {
        _ = inner.stop.cancelled() => return,
        selector = tokio::time::timeout(SELECT_WAIT, session.recv_frame()) => selector,
    };
    let result = match selector {
        Ok(Ok(selector)) if selector == SELECT_SESSION => {
            sessions::serve_tunnel(&inner, session).await
        }
        Ok(Ok(selector)) if selector == SELECT_EGRESS => {
            sessions::serve_egress(&inner, session).await
        }
        Ok(Ok(selector)) if selector == SELECT_PAIR => {
            tokio::select! {
                _ = inner.stop.cancelled() => Ok(()),
                result = offer::attempt(
                    &inner.offer,
                    session,
                    &inner.key,
                    &inner.options.host_name,
                    &inner.options.clients,
                    &inner.store_writes,
                ) => result.map(|_| ()),
            }
        }
        _ => Ok(()),
    };
    if let Err(error) = result {
        tracing::debug!("remote session ended: {error:#}");
    }
}
