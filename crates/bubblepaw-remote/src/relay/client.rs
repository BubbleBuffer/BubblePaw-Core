use super::{
    Limits,
    wire::{self, Wire},
};
use crate::{
    Error, Result,
    device::{DeviceId, DeviceKey},
    frame::{FrameIo, FrameReader, FrameWriter, SplitFrames},
};
use std::{collections::HashMap, sync::Arc};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
struct Command {
    frame: Wire,
    completion: Option<oneshot::Sender<Result<()>>>,
}

async fn command(sender: &mpsc::Sender<Command>, frame: Wire, limits: &Limits) -> Result<()> {
    let (completion, done) = oneshot::channel();
    tokio::time::timeout(limits.idle_timeout, async {
        sender
            .send(Command {
                frame,
                completion: Some(completion),
            })
            .await
            .map_err(|_| Error::Closed)?;
        done.await.map_err(|_| Error::Closed)?
    })
    .await
    .map_err(|_| Error::IdleTimeout)?
}

async fn write_command<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
    command: Command,
    limits: &Limits,
) -> Result<()> {
    match wire::send(ws, &command.frame, limits).await {
        Ok(()) => {
            if let Some(done) = command.completion {
                let _ = done.send(Ok(()));
            }
            Ok(())
        }
        Err(error) => {
            if let Some(done) = command.completion {
                let _ = done.send(Err(error));
            }
            Err(Error::Closed)
        }
    }
}

/// One routed ciphertext stream. Dropping it closes only its own session.
pub struct RelaySession {
    id: u64,
    peer: [u8; 32],
    outgoing: mpsc::Sender<Command>,
    incoming: mpsc::Receiver<Vec<u8>>,
    limits: Limits,
    guard: Arc<SessionGuard>,
}

/// Closes the session once its last half is dropped.
struct SessionGuard {
    id: u64,
    outgoing: mpsc::Sender<Command>,
    task: Option<JoinHandle<()>>,
    overload_shutdown: Option<watch::Sender<bool>>,
}

impl RelaySession {
    pub fn session_id(&self) -> u64 {
        self.id
    }
    pub fn peer_public_key(&self) -> [u8; 32] {
        self.peer
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if self
            .outgoing
            .try_send(Command {
                frame: Wire::Close(self.id),
                completion: None,
            })
            .is_err()
            && let Some(shutdown) = &self.overload_shutdown
        {
            let _ = shutdown.send(true);
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn send_data(
    outgoing: &mpsc::Sender<Command>,
    id: u64,
    frame: &[u8],
    limits: &Limits,
) -> Result<()> {
    if frame.len() > limits.max_frame_bytes {
        return Err(Error::OversizedFrame);
    }
    command(outgoing, Wire::Data(id, frame.to_vec()), limits).await
}

async fn recv_data(incoming: &mut mpsc::Receiver<Vec<u8>>, limits: &Limits) -> Result<Vec<u8>> {
    tokio::time::timeout(limits.idle_timeout, incoming.recv())
        .await
        .map_err(|_| Error::IdleTimeout)?
        .ok_or(Error::Closed)
}

#[async_trait::async_trait]
impl FrameIo for RelaySession {
    async fn send_frame(&mut self, frame: &[u8]) -> Result<()> {
        send_data(&self.outgoing, self.id, frame, &self.limits).await
    }

    async fn recv_frame(&mut self) -> Result<Vec<u8>> {
        recv_data(&mut self.incoming, &self.limits).await
    }
}

/// The receive direction of a split [`RelaySession`]. Like the whole session,
/// a receive with no data for the idle timeout fails.
pub struct RelayReader {
    incoming: mpsc::Receiver<Vec<u8>>,
    limits: Limits,
    _guard: Arc<SessionGuard>,
}

/// The send direction of a split [`RelaySession`].
pub struct RelayWriter {
    id: u64,
    outgoing: mpsc::Sender<Command>,
    limits: Limits,
    _guard: Arc<SessionGuard>,
}

#[async_trait::async_trait]
impl FrameReader for RelayReader {
    async fn recv_frame(&mut self) -> Result<Vec<u8>> {
        recv_data(&mut self.incoming, &self.limits).await
    }
}

#[async_trait::async_trait]
impl FrameWriter for RelayWriter {
    async fn send_frame(&mut self, frame: &[u8]) -> Result<()> {
        send_data(&self.outgoing, self.id, frame, &self.limits).await
    }
}

impl SplitFrames for RelaySession {
    type Reader = RelayReader;
    type Writer = RelayWriter;

    fn split_frames(self) -> (RelayReader, RelayWriter) {
        (
            RelayReader {
                incoming: self.incoming,
                limits: self.limits.clone(),
                _guard: self.guard.clone(),
            },
            RelayWriter {
                id: self.id,
                outgoing: self.outgoing,
                limits: self.limits,
                _guard: self.guard,
            },
        )
    }
}

/// A single host registration multiplexes independently encrypted client sessions.
pub struct Host {
    incoming: mpsc::Receiver<RelaySession>,
    outgoing: mpsc::Sender<Command>,
    task: JoinHandle<()>,
    limits: Limits,
}

impl Host {
    pub async fn accept(&mut self) -> Result<RelaySession> {
        loop {
            let session = self.incoming.recv().await.ok_or(Error::Closed)?;
            if session.incoming.is_closed() {
                continue;
            }
            command(&self.outgoing, Wire::Accept(session.id), &self.limits).await?;
            return Ok(session);
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn open_socket(url: &str, limits: &Limits) -> Result<Socket> {
    limits.validate()?;
    let (ws, _) = tokio::time::timeout(
        limits.idle_timeout,
        tokio_tungstenite::connect_async_with_config(url, Some(wire::config(limits)), false),
    )
    .await
    .map_err(|_| Error::IdleTimeout)??;
    Ok(ws)
}

async fn authenticate<S: AsyncRead + AsyncWrite + Unpin>(
    mut ws: WebSocketStream<S>,
    key: &DeviceKey,
    role: u8,
    target: Option<&DeviceId>,
    limits: &Limits,
) -> Result<WebSocketStream<S>> {
    limits.validate()?;
    wire::validate_config(&ws, limits)?;
    let Wire::Challenge(challenge) = wire::recv(&mut ws, limits).await? else {
        return Err(Error::AuthenticationFailed);
    };
    let target = target.map(ToString::to_string).unwrap_or_default();
    let signature = key.sign(&wire::signed(&challenge, role, &target));
    wire::send(
        &mut ws,
        &Wire::Auth {
            role,
            key: key.public_key(),
            signature,
            target,
        },
        limits,
    )
    .await?;
    Ok(ws)
}

/// Register the device ID derived from `key` as a reachable host.
pub async fn register(url: &str, key: &DeviceKey, limits: Limits) -> Result<Host> {
    register_socket(open_socket(url, &limits).await?, key, limits).await
}

/// Register using a caller-created WebSocket whose HTTP upgrade may carry
/// host-owned authentication headers/cookies. Credentials stay outside the URL.
pub async fn register_socket<S>(
    ws: WebSocketStream<S>,
    key: &DeviceKey,
    limits: Limits,
) -> Result<Host>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut ws = authenticate(ws, key, wire::REGISTER, None, &limits).await?;
    if !matches!(wire::recv(&mut ws, &limits).await?, Wire::Registered) {
        return Err(Error::AuthenticationFailed);
    }
    let (outgoing, outgoing_rx) = mpsc::channel(limits.queue_capacity);
    let (incoming_tx, incoming) = mpsc::channel(limits.pending_per_host);
    let (shutdown, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(host_loop(
        ws,
        outgoing.clone(),
        outgoing_rx,
        incoming_tx,
        limits.clone(),
        shutdown,
        shutdown_rx,
    ));
    Ok(Host {
        incoming,
        outgoing,
        task,
        limits,
    })
}

/// Ask the relay whether the host `target` is registered now. The relay
/// answers online or offline for the one ID named and nothing else; the query is
/// signed with `key` like a connect request and opens no session, so it never
/// disturbs the host. A relay that refuses this device fails with an error.
pub async fn probe(url: &str, key: &DeviceKey, target: &DeviceId, limits: Limits) -> Result<bool> {
    probe_socket(open_socket(url, &limits).await?, key, target, limits).await
}

/// [`probe`] over a WebSocket upgraded by the embedding account/session client.
pub async fn probe_socket<S>(
    ws: WebSocketStream<S>,
    key: &DeviceKey,
    target: &DeviceId,
    limits: Limits,
) -> Result<bool>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut ws = authenticate(ws, key, wire::PRESENCE, Some(target), &limits).await?;
    match wire::recv(&mut ws, &limits).await? {
        Wire::Presence(online) => Ok(online),
        _ => Err(Error::AuthenticationFailed),
    }
}

/// Wait until the target host accepts a new independently routed session.
pub async fn connect(
    url: &str,
    key: &DeviceKey,
    target: &DeviceId,
    limits: Limits,
) -> Result<RelaySession> {
    connect_socket(open_socket(url, &limits).await?, key, target, limits).await
}

/// Connect using a WebSocket upgraded by the embedding account/session client.
pub async fn connect_socket<S>(
    ws: WebSocketStream<S>,
    key: &DeviceKey,
    target: &DeviceId,
    limits: Limits,
) -> Result<RelaySession>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut ws = authenticate(ws, key, wire::CONNECT, Some(target), &limits).await?;
    let Wire::Connected(id, peer) = wire::recv(&mut ws, &limits).await? else {
        return Err(Error::AuthenticationFailed);
    };
    if DeviceId::from_public_key(&peer) != *target {
        return Err(Error::AuthenticationFailed);
    }
    let (outgoing, mut outgoing_rx) = mpsc::channel::<Command>(limits.queue_capacity);
    let (incoming_tx, incoming) = mpsc::channel(limits.queue_capacity);
    let task_limits = limits.clone();
    let task = tokio::spawn(async move {
        let result: Result<()> = async {
            let mut keepalive = wire::keepalive(&task_limits);
            loop {
                tokio::select! {
                    _ = keepalive.tick() => wire::ping(&mut ws, &task_limits).await?,
                    frame = wire::recv(&mut ws, &task_limits) => {
                        match frame? {
                            Wire::Data(session, bytes) if session == id => {
                                // This connection carries one session, so waiting
                                // for its reader pushes back on the relay instead
                                // of failing a burst; a reader stalled for the idle
                                // timeout still fails the session.
                                tokio::time::timeout(task_limits.idle_timeout, incoming_tx.send(bytes))
                                    .await
                                    .map_err(|_| Error::Limit("inbound queue"))?
                                    .map_err(|_| Error::Closed)?;
                            }
                            Wire::Close(session) if session == id => return Ok(()),
                            _ => return Err(Error::Protocol("unexpected session frame")),
                        }
                    }
                    Some(frame) = outgoing_rx.recv() => {
                        let close = matches!(frame.frame, Wire::Close(_));
                        write_command(&mut ws, frame, &task_limits).await?;
                        if close { return Ok(()); }
                    }
                }
            }
        }.await;
        if let Err(error) = result {
            tracing::warn!("relay client session {id} ended: {error}");
        }
    });
    Ok(RelaySession {
        id,
        peer,
        guard: Arc::new(SessionGuard {
            id,
            outgoing: outgoing.clone(),
            task: Some(task),
            overload_shutdown: None,
        }),
        outgoing,
        incoming,
        limits,
    })
}

/// Hand a frame to one session's reader, waiting for room when its queue is
/// full instead of dropping the frame or the session: the reader may be slow
/// for a while (a multi-chunk message arrives as a burst). Writes for the other
/// sessions go on meanwhile. False when the reader is gone or made no room for
/// the idle timeout; the caller then closes that session.
async fn deliver<S>(
    ws: &mut WebSocketStream<S>,
    commands: &mut mpsc::Receiver<Command>,
    sessions: &mut HashMap<u64, mpsc::Sender<Vec<u8>>>,
    sender: &mpsc::Sender<Vec<u8>>,
    bytes: Vec<u8>,
    limits: &Limits,
) -> Result<bool>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let wait = tokio::time::sleep(limits.idle_timeout);
    tokio::pin!(wait);
    let reserve = sender.reserve();
    tokio::pin!(reserve);
    loop {
        tokio::select! {
            permit = &mut reserve => {
                return Ok(match permit {
                    Ok(permit) => {
                        permit.send(bytes);
                        true
                    }
                    Err(_) => false,
                });
            }
            Some(frame) = commands.recv() => {
                if let Wire::Close(id) = &frame.frame { sessions.remove(id); }
                write_command(ws, frame, limits).await?;
            }
            _ = &mut wait => return Ok(false),
        }
    }
}

async fn host_loop<S>(
    mut ws: WebSocketStream<S>,
    outgoing: mpsc::Sender<Command>,
    mut commands: mpsc::Receiver<Command>,
    incoming: mpsc::Sender<RelaySession>,
    limits: Limits,
    shutdown: watch::Sender<bool>,
    mut shutdown_rx: watch::Receiver<bool>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let mut sessions: HashMap<u64, mpsc::Sender<Vec<u8>>> = HashMap::new();
    let result: Result<()> = async {
        let mut keepalive = wire::keepalive(&limits);
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => return Ok(()),
                _ = keepalive.tick() => wire::ping(&mut ws, &limits).await?,
                frame = wire::recv(&mut ws, &limits) => match frame? {
                    Wire::Incoming(id, peer) => {
                        if sessions.len() >= limits.sessions_per_host || sessions.contains_key(&id) {
                            return Err(Error::Limit("host sessions"));
                        }
                        let (sender, receiver) = mpsc::channel(limits.queue_capacity);
                        sessions.insert(id, sender);
                        let guard = Arc::new(SessionGuard {
                            id, outgoing: outgoing.clone(), task: None,
                            overload_shutdown: Some(shutdown.clone()),
                        });
                        let session = RelaySession {
                            id, peer, outgoing: outgoing.clone(), incoming: receiver,
                            limits: limits.clone(), guard,
                        };
                        if incoming.try_send(session).is_err() {
                            sessions.remove(&id);
                            wire::send(&mut ws, &Wire::Close(id), &limits).await?;
                        }
                    }
                    Wire::Data(id, bytes) => {
                        let Some(sender) = sessions.get(&id).cloned() else { continue; };
                        if !deliver(&mut ws, &mut commands, &mut sessions, &sender, bytes, &limits).await? {
                            tracing::warn!("host closes session {id}: its reader left {} frames unread for {:?}", sender.max_capacity(), limits.idle_timeout);
                            sessions.remove(&id);
                            wire::send(&mut ws, &Wire::Close(id), &limits).await?;
                        }
                    }
                    Wire::Close(id) => { sessions.remove(&id); }
                    _ => return Err(Error::Protocol("unexpected host frame")),
                },
                Some(frame) = commands.recv() => {
                    if let Wire::Close(id) = &frame.frame { sessions.remove(id); }
                    write_command(&mut ws, frame, &limits).await?;
                }
            }
        }
    }.await;
    if let Err(error) = result {
        tracing::warn!("relay host connection ended: {error}");
    }
}
