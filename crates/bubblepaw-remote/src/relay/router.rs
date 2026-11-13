use super::{
    RelayAdmission, RelayConfig, RelayRole, admission,
    lanes::Lanes,
    outbox::{self, Outbox, Sent},
    routing::{Forward, Role, State},
    wire::{self, Wire},
};
use crate::{Error, Result};
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    sync::{Notify, watch},
};
use tokio_tungstenite::{WebSocketStream, tungstenite::protocol::WebSocketConfig};

#[derive(Default)]
struct Connections {
    routing: State,
    by_ip: HashMap<IpAddr, usize>,
    next: u64,
    active: usize,
    closed: bool,
}

struct Inner {
    config: RelayConfig,
    state: Mutex<Connections>,
    shutdown: watch::Sender<bool>,
    drained: Notify,
}

/// Shared routing and limits for standalone listeners or an embedding hub.
/// Each `serve_websocket` call belongs to the caller's task; the router starts no
/// detached tasks. Cancellation releases connection/session resources by RAII.
#[derive(Clone)]
pub struct RelayRouter {
    inner: Arc<Inner>,
}

struct Connection {
    router: RelayRouter,
    id: u64,
    ip: IpAddr,
    role: Option<Role>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        let mut state = self
            .router
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(role) = &self.role {
            state.routing.detach(role, self.id);
        }
        if let Some(count) = state.by_ip.get_mut(&self.ip) {
            *count -= 1;
            if *count == 0 {
                state.by_ip.remove(&self.ip);
            }
        }
        state.active -= 1;
        self.router.inner.drained.notify_waiters();
    }
}

impl RelayRouter {
    pub fn new(config: RelayConfig) -> Result<Self> {
        config.limits.validate()?;
        let (shutdown, _) = watch::channel(false);
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                state: Mutex::new(Connections::default()),
                shutdown,
                drained: Notify::new(),
            }),
        })
    }

    /// Apply this while accepting an HTTP upgrade, before any frame buffering.
    pub fn websocket_config(&self) -> WebSocketConfig {
        wire::config(&self.inner.config.limits)
    }

    fn reserve(&self, ip: IpAddr) -> Result<Connection> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.closed {
            return Err(Error::Closed);
        }
        if state.by_ip.get(&ip).copied().unwrap_or(0) >= self.inner.config.limits.connections_per_ip
        {
            return Err(Error::Limit("connections per IP"));
        }
        state.next = state
            .next
            .checked_add(1)
            .ok_or(Error::Limit("connection ids"))?;
        *state.by_ip.entry(ip).or_default() += 1;
        state.active += 1;
        Ok(Connection {
            router: self.clone(),
            id: state.next,
            ip,
            role: None,
        })
    }

    /// Serve one socket accepted by a trusted HTTP route. `admission` may capture
    /// that route's authenticated OAuth account/session; relay frames never set
    /// it. Supply the trusted peer IP, without trusting arbitrary request headers.
    pub async fn serve_websocket<S>(
        &self,
        ws: WebSocketStream<S>,
        peer_ip: IpAddr,
        admission: Arc<dyn RelayAdmission>,
    ) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let connection = self.reserve(peer_ip)?;
        self.serve_reserved(ws, connection, admission).await
    }

    pub(super) async fn serve_tcp(&self, stream: TcpStream, ip: IpAddr) -> Result<()> {
        let connection = self.reserve(ip)?;
        let mut stop = self.inner.shutdown.subscribe();
        if *stop.borrow() {
            return Err(Error::Closed);
        }
        let upgrade =
            tokio_tungstenite::accept_async_with_config(stream, Some(self.websocket_config()));
        let ws = tokio::select! {
            _ = stop.changed() => return Err(Error::Closed),
            result = tokio::time::timeout(self.inner.config.limits.idle_timeout, upgrade) => {
                result.map_err(|_| Error::IdleTimeout)??
            }
        };
        self.serve_reserved(ws, connection, self.inner.config.admission.clone())
            .await
    }

    async fn serve_reserved<S>(
        &self,
        ws: WebSocketStream<S>,
        connection: Connection,
        policy: Arc<dyn RelayAdmission>,
    ) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        wire::validate_config(&ws, &self.inner.config.limits)?;
        let mut stop = self.inner.shutdown.subscribe();
        if *stop.borrow() {
            return Err(Error::Closed);
        }
        // Cancellation covers authentication, policy rejection writes and data
        // writes as well as reads. Dropping the protocol future releases its guard.
        let id = connection.id;
        let result = tokio::select! {
            _ = stop.changed() => Err(Error::Closed),
            result = self.run_protocol(ws, connection, policy) => result,
        };
        if let Err(error) = &result {
            tracing::warn!("relay connection {id} ended: {error}");
        }
        result
    }

    async fn run_protocol<S>(
        &self,
        mut ws: WebSocketStream<S>,
        mut connection: Connection,
        policy: Arc<dyn RelayAdmission>,
    ) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let limits = &self.inner.config.limits;
        let (proof, presence) = admission::authenticate(&mut ws, limits).await?;
        if !admission::permitted(policy.as_ref(), &proof) {
            wire::send(&mut ws, &Wire::Reject, limits).await?;
            return Err(Error::AuthenticationFailed);
        }
        if presence {
            // A presence query opens no session: it learns online or offline for
            // the one host it names, answers once and ends.
            let online = proof.target.as_ref().is_some_and(|target| {
                self.inner
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .routing
                    .is_online(&target.to_string())
            });
            wire::send(&mut ws, &Wire::Presence(online), limits).await?;
            return Ok(());
        }
        let (outbox, mut inbox) = outbox::channel(limits.queue_capacity);
        let role_byte = if proof.role == RelayRole::Host {
            wire::REGISTER
        } else {
            wire::CONNECT
        };
        let attached = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .routing
            .attach(
                role_byte,
                proof.public_key,
                proof.target.map(|id| id.to_string()).unwrap_or_default(),
                connection.id,
                outbox,
                limits,
            );
        let role = match attached {
            Ok(role) => role,
            Err(error) => {
                wire::send(&mut ws, &Wire::Reject, limits).await?;
                return Err(error);
            }
        };
        connection.role = Some(role);
        let role = connection.role.as_ref().ok_or(Error::Closed)?;
        if matches!(role, Role::Host(_)) {
            wire::send(&mut ws, &Wire::Registered, limits).await?;
        }
        // Frames from a host fan out to its sessions through lanes: a session whose
        // queue is full waits there while the host's other sessions go on.
        let mut lanes = Lanes::<Outbox<Wire>>::new(limits.queue_capacity, limits.idle_timeout);
        loop {
            tokio::select! {
                incoming = wire::recv(&mut ws, limits), if !lanes.blocked() => {
                    let forward = self.inner.state.lock().unwrap_or_else(|error| error.into_inner())
                        .routing.route(role, incoming?, &self.inner.config.ciphertext_observer)?;
                    let Some(Forward { to, frame, control, session }) = forward else { continue };
                    if matches!(role, Role::Host(_)) {
                        lanes.send(session, &to, frame, control);
                    } else {
                        // A client waits for the host's queue on its own connection;
                        // no other session is held up by it.
                        match to.send_data(frame, limits.idle_timeout).await {
                            Sent::Queued => {}
                            Sent::Gone => return Err(Error::Closed),
                            Sent::Stalled => {
                                tracing::warn!("relay drops a stalled peer: the host's queue stayed full for {:?}", limits.idle_timeout);
                                return Err(Error::Limit("host data"));
                            }
                        }
                    }
                }
                frame = inbox.recv() => {
                    let close = matches!(frame, Wire::Close(_));
                    wire::send(&mut ws, &frame, limits).await?;
                    if close && matches!(role, Role::Client(_, _)) { return Ok(()); }
                }
                stalled = lanes.next() => if let Some(stalled) = stalled {
                    tracing::warn!("relay drops a stalled peer: a client's queue stayed full for {:?}", limits.idle_timeout);
                    if let Role::Host(host) = role {
                        self.inner.state.lock().unwrap_or_else(|error| error.into_inner())
                            .routing.drop_stalled_client(host, stalled.id);
                    }
                    stalled.dest.control(Wire::Close(stalled.id));
                }
            }
        }
    }

    /// Stop admission and wait for every active serving future to release its
    /// socket and routing state. The embedding host retains task ownership.
    pub async fn shutdown(&self) {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .closed = true;
        self.inner.shutdown.send_replace(true);
        loop {
            let notified = self.inner.drained.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self
                .inner
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .active
                == 0
            {
                return;
            }
            notified.await;
        }
    }
}
