use super::{
    Limits,
    wire::{self, Wire},
};
use crate::{
    Error, Result,
    device::{DeviceId, DeviceKey},
    frame::FrameIo,
};
use std::collections::HashMap;
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

impl Drop for RelaySession {
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

#[async_trait::async_trait]
impl FrameIo for RelaySession {
    async fn send_frame(&mut self, frame: &[u8]) -> Result<()> {
        if frame.len() > self.limits.max_frame_bytes {
            return Err(Error::OversizedFrame);
        }
        command(
            &self.outgoing,
            Wire::Data(self.id, frame.to_vec()),
            &self.limits,
        )
        .await
    }

    async fn recv_frame(&mut self) -> Result<Vec<u8>> {
        tokio::time::timeout(self.limits.idle_timeout, self.incoming.recv())
            .await
            .map_err(|_| Error::IdleTimeout)?
            .ok_or(Error::Closed)
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
    target: Option<&DeviceId>,
    limits: &Limits,
) -> Result<WebSocketStream<S>> {
    limits.validate()?;
    wire::validate_config(&ws, limits)?;
    let Wire::Challenge(challenge) = wire::recv(&mut ws, limits).await? else {
        return Err(Error::AuthenticationFailed);
    };
    let role = if target.is_some() {
        wire::CONNECT
    } else {
        wire::REGISTER
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
    let mut ws = authenticate(ws, key, None, &limits).await?;
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
    let mut ws = authenticate(ws, key, Some(target), &limits).await?;
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
            loop {
                tokio::select! {
                    frame = wire::recv(&mut ws, &task_limits) => {
                        match frame? {
                            Wire::Data(session, bytes) if session == id => {
                                incoming_tx.try_send(bytes).map_err(|_| Error::Limit("inbound queue"))?;
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
        let _ = result;
    });
    Ok(RelaySession {
        id,
        peer,
        outgoing,
        incoming,
        limits,
        task: Some(task),
        overload_shutdown: None,
    })
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
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => return Ok(()),
                frame = wire::recv(&mut ws, &limits) => match frame? {
                    Wire::Incoming(id, peer) => {
                        if sessions.len() >= limits.sessions_per_host || sessions.contains_key(&id) {
                            return Err(Error::Limit("host sessions"));
                        }
                        let (sender, receiver) = mpsc::channel(limits.queue_capacity);
                        sessions.insert(id, sender);
                        let session = RelaySession {
                            id, peer, outgoing: outgoing.clone(), incoming: receiver,
                            limits: limits.clone(), task: None, overload_shutdown: Some(shutdown.clone()),
                        };
                        if incoming.try_send(session).is_err() {
                            sessions.remove(&id);
                            wire::send(&mut ws, &Wire::Close(id), &limits).await?;
                        }
                    }
                    Wire::Data(id, bytes) => {
                        let Some(sender) = sessions.get(&id) else { continue; };
                        if sender.try_send(bytes).is_err() {
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
    let _ = result;
}
