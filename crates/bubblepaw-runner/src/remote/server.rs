//! The host side: registered at the relay, it admits paired devices into
//! tunnels and serves each through the host API with the device's profile.
use super::offer::{self, OfferSlot, PairingOffer};
use super::{
    Limits, PairedDevice, SELECT_PAIR, SELECT_SESSION, adapter, api_profile, list_devices,
    load_or_create_key, revoke_device,
};
use crate::host_api::{HostCtx, Profile};
use anyhow::{Context, Result, anyhow};
use bubblepaw_core::CancellationToken;
use bubblepaw_remote::device::{DeviceId, DeviceKey};
use bubblepaw_remote::frame::FrameIo;
use bubblepaw_remote::relay::{self, RelaySession};
use bubblepaw_remote::store::{PairStore, PairedClient};
use bubblepaw_remote::trust::DeviceListAuthorizer;
use bubblepaw_remote::tunnel;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// How long a new relay session may take to say what it wants.
const SELECT_WAIT: Duration = Duration::from_secs(10);
/// How long tunnel admission may take.
const ADMIT_WAIT: Duration = Duration::from_secs(30);
/// How often the paired-device store is checked for changes by other processes.
const STORE_POLL: Duration = Duration::from_secs(1);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Where tunnels are served.
#[derive(Clone)]
pub enum SessionBackend {
    /// In this process, on the host's shared context.
    Context(Box<HostCtx>),
    /// Through the host that owns this data directory, over its control
    /// endpoint (`attach.session` with the device's profile).
    Attach(PathBuf),
}

#[derive(Clone, Debug)]
pub struct RemoteServeOptions {
    /// The relay's `wss://` (or `ws://`) URL.
    pub relay: String,
    /// The host device key, created on first use.
    pub key_file: PathBuf,
    /// The paired-device store.
    pub clients: PathBuf,
    /// The name devices see while pairing.
    pub host_name: String,
    pub limits: Limits,
}

impl RemoteServeOptions {
    /// Options with the standard paths in `data_dir` and default limits.
    pub fn in_data_dir(data_dir: &std::path::Path, relay: String, host_name: String) -> Self {
        let paths = super::RemotePaths::in_data_dir(data_dir);
        Self {
            relay,
            key_file: paths.key_file,
            clients: paths.clients,
            host_name,
            limits: Limits::default(),
        }
    }
}

/// The relay registration's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteStatus {
    Connecting,
    Online,
    /// The last attempt failed; the server retries with backoff.
    Offline,
}

struct Live {
    device: String,
    profile: Profile,
    stop: CancellationToken,
}

struct Inner {
    options: RemoteServeOptions,
    key: DeviceKey,
    backend: Option<SessionBackend>,
    authorizer: DeviceListAuthorizer,
    stamp: Mutex<Option<(SystemTime, u64)>>,
    tunnels: Mutex<HashMap<u64, Live>>,
    next: AtomicU64,
    offer: OfferSlot,
    store_writes: tokio::sync::Mutex<()>,
    stop: CancellationToken,
}

/// A running remote server. Dropping it stops the server and closes its tunnels.
pub struct RemoteServer {
    inner: Arc<Inner>,
    status: watch::Receiver<RemoteStatus>,
    tasks: Vec<JoinHandle<()>>,
}

impl RemoteServer {
    /// Load or create the host key, read the paired devices and start
    /// registering at the relay. With no backend, the server only pairs and
    /// refuses tunnels.
    pub fn start(options: RemoteServeOptions, backend: Option<SessionBackend>) -> Result<Self> {
        let key = load_or_create_key(&options.key_file, &options.host_name)?;
        let inner = Arc::new(Inner {
            authorizer: DeviceListAuthorizer::new(std::iter::empty())
                .map_err(|error| anyhow!("{error}"))?,
            options,
            key,
            backend,
            stamp: Mutex::new(None),
            tunnels: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
            offer: OfferSlot::default(),
            store_writes: tokio::sync::Mutex::new(()),
            stop: CancellationToken::new(),
        });
        inner.refresh(true)?;
        let (status_tx, status) = watch::channel(RemoteStatus::Connecting);
        let relay_task = tokio::spawn(relay_loop(inner.clone(), status_tx));
        let poll_task = tokio::spawn(poll_store(inner.clone()));
        Ok(Self {
            inner,
            status,
            tasks: vec![relay_task, poll_task],
        })
    }

    /// This host's device ID, which devices need to reach it.
    pub fn host_id(&self) -> String {
        self.inner.key.id().to_string()
    }

    pub fn status(&self) -> RemoteStatus {
        *self.status.borrow()
    }

    /// A receiver that sees every status change.
    pub fn watch_status(&self) -> watch::Receiver<RemoteStatus> {
        self.status.clone()
    }

    /// Wait until the relay accepted the registration.
    pub async fn wait_online(&self, timeout: Duration) -> Result<()> {
        let mut status = self.status.clone();
        tokio::time::timeout(
            timeout,
            status.wait_for(|status| *status == RemoteStatus::Online),
        )
        .await
        .context("the relay did not accept the host in time")?
        .map_err(|_| anyhow!("the remote server stopped"))?;
        Ok(())
    }

    /// Open a pairing offer for a device that will get `profile`. Replaces an
    /// earlier offer.
    pub async fn pair(&self, profile: Profile) -> Result<PairingOffer> {
        let id = self.inner.next.fetch_add(1, Ordering::Relaxed);
        PairingOffer::install(
            &self.inner.offer,
            id,
            &self.inner.options.relay,
            &self.inner.key.id(),
            profile,
        )
        .await
    }

    pub fn devices(&self) -> Result<Vec<PairedDevice>> {
        list_devices(&self.inner.options.clients)
    }

    /// Remove a device and close its live tunnels now. Returns false when no
    /// such device was paired.
    pub async fn revoke(&self, id: &str) -> Result<bool> {
        let removed = {
            let _writing = self.inner.store_writes.lock().await;
            revoke_device(&self.inner.options.clients, id)?
        };
        self.inner.refresh(true)?;
        Ok(removed)
    }

    /// The number of open tunnels.
    pub fn tunnel_count(&self) -> usize {
        self.inner.tunnels.lock().unwrap().len()
    }

    /// Stop registering, close every tunnel and wait for the server's tasks.
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
            if devices.get(&live.device) != Some(&live.profile) {
                live.stop.cancel();
            }
        }
        Ok(())
    }
}

async fn relay_loop(inner: Arc<Inner>, status: watch::Sender<RemoteStatus>) {
    let mut backoff = BACKOFF_MIN;
    loop {
        status.send_replace(RemoteStatus::Connecting);
        let registered = tokio::select! {
            _ = inner.stop.cancelled() => return,
            host = relay::register(&inner.options.relay, &inner.key, inner.options.limits.clone()) => host,
        };
        match registered {
            Ok(mut host) => {
                backoff = BACKOFF_MIN;
                status.send_replace(RemoteStatus::Online);
                loop {
                    let session = tokio::select! {
                        _ = inner.stop.cancelled() => return,
                        session = host.accept() => session,
                    };
                    match session {
                        Ok(session) => {
                            tokio::spawn(handle(inner.clone(), session));
                        }
                        Err(error) => {
                            tracing::warn!("relay registration ended: {error}");
                            break;
                        }
                    }
                }
            }
            Err(error) => tracing::warn!("could not register at the relay: {error}"),
        }
        status.send_replace(RemoteStatus::Offline);
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

async fn handle(inner: Arc<Inner>, mut session: RelaySession) {
    let selector = tokio::select! {
        _ = inner.stop.cancelled() => return,
        selector = tokio::time::timeout(SELECT_WAIT, session.recv_frame()) => selector,
    };
    let result = match selector {
        Ok(Ok(selector)) if selector == SELECT_SESSION => serve_tunnel(&inner, session).await,
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

async fn serve_tunnel(inner: &Arc<Inner>, session: RelaySession) -> Result<()> {
    // Admit against the store as it is now, not as of the last poll.
    inner.refresh(false)?;
    let tunnel = tokio::time::timeout(
        ADMIT_WAIT,
        tunnel::accept(session, &inner.key, &inner.authorizer),
    )
    .await
    .context("tunnel admission timed out")?
    .map_err(|error| anyhow!("tunnel refused: {error}"))?;
    let peer = tunnel
        .authorization()
        .cloned()
        .context("tunnel admitted without a grant")?;
    let Some(backend) = inner.backend.clone() else {
        return Ok(());
    };
    let profile = api_profile(peer.profile);
    let stop = CancellationToken::new();
    let id = inner.next.fetch_add(1, Ordering::Relaxed);
    inner.tunnels.lock().unwrap().insert(
        id,
        Live {
            device: peer.id.to_string(),
            profile,
            stop: stop.clone(),
        },
    );
    // A revoke or shutdown between admission and registration is caught here.
    inner.refresh(true).ok();
    if inner.stop.is_cancelled() {
        stop.cancel();
    }
    let result = async {
        let (reader, writer) = tunnel.split().map_err(|error| anyhow!("{error}"))?;
        match &backend {
            SessionBackend::Context(ctx) => {
                adapter::serve_in_process(ctx, profile, reader, writer, stop.clone()).await
            }
            SessionBackend::Attach(data_dir) => {
                adapter::serve_attached(data_dir, profile, reader, writer, stop.clone()).await
            }
        }
    }
    .await;
    stop.cancel();
    inner.tunnels.lock().unwrap().remove(&id);
    result
}
