//! A host-API JSON-RPC client for one session: in process on a [`HostCtx`],
//! or over a host control endpoint after `attach.session`. Both run the same
//! session loop as stdio (`rpc_session`), so replies and notifications have the
//! stdio shapes. The client adds no methods; every call goes to the table.
use crate::host_api::{HostCtx, Profile, ReadScope};
use crate::host_transport::{self, HOST_PROTOCOL_VERSION};
use crate::rpc_session::{self, Incoming, RECORD_LIMIT};
use bubblepaw_core::CancellationToken;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc, oneshot};

/// Notifications a subscriber may fall behind by before it sees `events.gap`.
const NOTIFICATION_BUFFER: usize = 1024;
/// How long `close` waits for an attached session to end on its own.
const CLOSE_WAIT: Duration = Duration::from_secs(2);

/// Why a call or a connection failed.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientError {
    /// The host answered with a JSON-RPC error.
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    /// The session ended before the reply arrived. The request may have run;
    /// it is never sent again.
    Disconnected,
    /// The host sent a frame this client cannot read, or a request was too large.
    Protocol(String),
    /// No host could be reached, or the host could not open a session.
    Host(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rpc { code, message, .. } => write!(f, "host error {code}: {message}"),
            Self::Disconnected => f.write_str("the host session ended before the reply"),
            Self::Protocol(message) => write!(f, "host protocol error: {message}"),
            Self::Host(message) => write!(f, "host unavailable: {message}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl ClientError {
    fn host(error: anyhow::Error) -> Self {
        Self::Host(format!("{error:#}"))
    }
}

/// Notification frames (`event`, `events.gap`) as JSON values.
pub struct Notifications {
    receiver: broadcast::Receiver<Value>,
    /// The last durable sequence this subscriber saw, for `events.gap`.
    after: u64,
}

impl Notifications {
    /// The next notification, or `None` once the session has ended. A
    /// subscriber that falls behind gets one `events.gap` in the stdio shape;
    /// its `after` is the last durable event sequence this subscriber saw
    /// (0 before the first), and `events.after` recovers from there.
    pub async fn recv(&mut self) -> Option<Value> {
        match self.receiver.recv().await {
            Ok(notification) => {
                let params = &notification["params"];
                match notification["method"].as_str() {
                    Some("event") if params["durable"] == true => {
                        if let Some(sequence) = params["sequence"].as_u64() {
                            self.after = self.after.max(sequence);
                        }
                    }
                    Some("events.gap") => {
                        if let Some(after) = params["after"].as_u64() {
                            self.after = after;
                        }
                    }
                    _ => {}
                }
                Some(notification)
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => Some(json!({
                "jsonrpc": "2.0",
                "method": "events.gap",
                "params": {"skipped": skipped, "after": self.after, "recover_with": "events.after"},
            })),
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }
}

struct Shared {
    outgoing: mpsc::Sender<Incoming>,
    /// Calls waiting for their reply. `None` once the session has ended.
    pending: Mutex<Option<HashMap<u64, oneshot::Sender<Value>>>>,
    next_id: AtomicU64,
    notifications: broadcast::Sender<Value>,
}

impl Shared {
    fn new(outgoing: mpsc::Sender<Incoming>, first_id: u64) -> Arc<Self> {
        Arc::new(Self {
            outgoing,
            pending: Mutex::new(Some(HashMap::new())),
            next_id: AtomicU64::new(first_id),
            notifications: broadcast::channel(NOTIFICATION_BUFFER).0,
        })
    }

    /// Route one frame from the host: notifications to subscribers, replies to
    /// their call. A reply without a known id has no caller and is dropped.
    fn deliver(&self, frame: Value) {
        if frame.get("method").is_some() {
            let _ = self.notifications.send(frame);
            return;
        }
        let Some(id) = frame.get("id").and_then(Value::as_u64) else {
            return;
        };
        let waiting = self
            .pending
            .lock()
            .unwrap()
            .as_mut()
            .and_then(|pending| pending.remove(&id));
        if let Some(waiting) = waiting {
            let _ = waiting.send(frame);
        }
    }

    /// The session ended: every waiting call fails with `Disconnected`.
    fn disconnect(&self) {
        self.pending.lock().unwrap().take();
    }

    fn forget(&self, id: u64) {
        if let Some(pending) = self.pending.lock().unwrap().as_mut() {
            pending.remove(&id);
        }
    }
}

/// One host-API session. Calls may run concurrently through `&self`.
pub struct HostClient {
    shared: Arc<Shared>,
    stop: CancellationToken,
    /// The session (in process) or the writer (attached), then the reader.
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl HostClient {
    /// A session on `ctx` in this process, with `ReadScope::Host`. Must be
    /// called inside a Tokio runtime.
    pub fn in_process(mut ctx: HostCtx) -> Self {
        ctx.read_scope = ReadScope::Host;
        let (outgoing, incoming) = mpsc::channel(16);
        let (output, mut frames) = mpsc::channel::<Value>(128);
        let stop = CancellationToken::new();
        let shared = Shared::new(outgoing, 1);
        let session = {
            let stop = stop.clone();
            tokio::spawn(async move {
                let _ = rpc_session::serve(
                    &ctx,
                    Profile::Full,
                    incoming,
                    output,
                    &stop,
                    std::future::pending::<()>,
                )
                .await;
            })
        };
        let router = {
            let shared = shared.clone();
            tokio::spawn(async move {
                while let Some(frame) = frames.recv().await {
                    shared.deliver(frame);
                }
                shared.disconnect();
            })
        };
        Self {
            shared,
            stop,
            tasks: vec![session, router],
        }
    }

    /// A session on the host that publishes in `data_dir`, waiting up to
    /// `timeout` for it to listen.
    pub async fn attach(data_dir: &Path, timeout: Duration) -> Result<Self, ClientError> {
        let connection = host_transport::connect(data_dir, timeout)
            .await
            .map_err(ClientError::host)?;
        let (read, mut write) = tokio::io::split(connection);
        let mut reader = BufReader::new(read);
        let open = json!({"jsonrpc":"2.0","id":0,"method":"attach.session","params":{}});
        write_frame(&mut write, &open)
            .await
            .map_err(|error| ClientError::Host(error.to_string()))?;
        let opened = read_frame(&mut reader)
            .await?
            .ok_or_else(|| ClientError::Host("the host closed before the session opened".into()))?;
        let result = reply_result(opened)?;
        if result["protocol_version"] != HOST_PROTOCOL_VERSION {
            return Err(ClientError::Protocol(format!(
                "the host opened protocol version {}, but this client speaks {HOST_PROTOCOL_VERSION}",
                result["protocol_version"]
            )));
        }
        Ok(Self::over(reader, write))
    }

    fn over<R, W>(mut reader: BufReader<R>, mut write: W) -> Self
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (outgoing, mut records) = mpsc::channel::<Incoming>(16);
        let stop = CancellationToken::new();
        let shared = Shared::new(outgoing, 1);
        let writer = {
            let stop = stop.clone();
            tokio::spawn(async move {
                loop {
                    let record = tokio::select! {
                        _ = stop.cancelled() => break,
                        record = records.recv() => record,
                    };
                    let Some(Ok(Some(mut bytes))) = record else {
                        break;
                    };
                    bytes.push(b'\n');
                    if write.write_all(&bytes).await.is_err() || write.flush().await.is_err() {
                        break;
                    }
                }
                // End of input ends the host's session loop.
                let _ = write.shutdown().await;
            })
        };
        let reader = {
            let shared = shared.clone();
            tokio::spawn(async move {
                while let Ok(Some(frame)) = read_frame(&mut reader).await {
                    shared.deliver(frame);
                }
                shared.disconnect();
            })
        };
        Self {
            shared,
            stop,
            tasks: vec![writer, reader],
        }
    }

    /// Call one host-API method and return its `result`.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, ClientError> {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let frame = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let bytes =
            serde_json::to_vec(&frame).map_err(|error| ClientError::Protocol(error.to_string()))?;
        if bytes.len() >= RECORD_LIMIT {
            return Err(ClientError::Protocol("request exceeds 1 MiB".into()));
        }
        let (send, reply) = oneshot::channel();
        match self.shared.pending.lock().unwrap().as_mut() {
            Some(pending) => pending.insert(id, send),
            None => return Err(ClientError::Disconnected),
        };
        if self.shared.outgoing.send(Ok(Some(bytes))).await.is_err() {
            self.shared.forget(id);
            return Err(ClientError::Disconnected);
        }
        reply_result(reply.await.map_err(|_| ClientError::Disconnected)?)
    }

    /// Every notification from now on: `event`, and `events.gap` when the host
    /// or this subscriber falls behind.
    pub fn subscribe(&self) -> Notifications {
        Notifications {
            receiver: self.shared.notifications.subscribe(),
            after: 0,
        }
    }

    /// End the session. The host itself keeps running.
    pub async fn close(mut self) {
        self.stop.cancel();
        let tasks = std::mem::take(&mut self.tasks);
        drop(self);
        for mut task in tasks {
            if tokio::time::timeout(CLOSE_WAIT, &mut task).await.is_err() {
                task.abort();
                let _ = task.await;
            }
        }
    }
}

impl Drop for HostClient {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

fn reply_result(mut reply: Value) -> Result<Value, ClientError> {
    if let Some(error) = reply.get("error") {
        return Err(ClientError::Rpc {
            code: error["code"].as_i64().unwrap_or_default(),
            message: error["message"].as_str().unwrap_or_default().to_owned(),
            data: error.get("data").cloned(),
        });
    }
    reply
        .get_mut("result")
        .map(Value::take)
        .ok_or_else(|| ClientError::Protocol(format!("reply without a result: {reply}")))
}

async fn read_frame<R>(reader: &mut BufReader<R>) -> Result<Option<Value>, ClientError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let Some(record) = rpc_session::read_record_async(reader)
        .await
        .map_err(|error| ClientError::Protocol(format!("{error:#}")))?
    else {
        return Ok(None);
    };
    serde_json::from_slice(&record)
        .map(Some)
        .map_err(|error| ClientError::Protocol(error.to_string()))
}

async fn write_frame<W: tokio::io::AsyncWrite + Unpin>(
    write: &mut W,
    frame: &Value,
) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(frame)?;
    bytes.push(b'\n');
    write.write_all(&bytes).await?;
    write.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replies_reach_their_call_and_notifications_their_subscribers() {
        let (client, host) = tokio::io::duplex(64 * 1024);
        let (read, write) = tokio::io::split(client);
        let session = HostClient::over(BufReader::new(read), write);
        let mut notifications = session.subscribe();
        let (host_read, mut host_write) = tokio::io::split(host);
        let host = tokio::spawn(async move {
            let mut reader = BufReader::new(host_read);
            let request = read_frame(&mut reader).await.unwrap().unwrap();
            let event =
                json!({"jsonrpc":"2.0","method":"event","params":{"durable":true,"sequence":7}});
            write_frame(&mut host_write, &event).await.unwrap();
            let reply =
                json!({"jsonrpc":"2.0","id":request["id"],"result":{"method":request["method"]}});
            write_frame(&mut host_write, &reply).await.unwrap();
            let request = read_frame(&mut reader).await.unwrap().unwrap();
            let error =
                json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32601,"message":"no"}});
            write_frame(&mut host_write, &error).await.unwrap();
            // Ending the stream fails the third call without a reply.
            read_frame(&mut reader).await.unwrap().unwrap()
        });
        assert_eq!(
            session.call("ping", json!({})).await.unwrap(),
            json!({"method":"ping"})
        );
        assert_eq!(notifications.recv().await.unwrap()["params"]["sequence"], 7);
        assert!(matches!(
            session.call("missing", json!({})).await,
            Err(ClientError::Rpc { code: -32601, .. })
        ));
        let third = session.call("never", json!({}));
        let (third, unanswered) = tokio::join!(third, host);
        assert_eq!(third, Err(ClientError::Disconnected));
        assert_eq!(unanswered.unwrap()["method"], "never");
        assert!(matches!(
            session.call("after", json!({})).await,
            Err(ClientError::Disconnected)
        ));
        session.close().await;
    }

    #[tokio::test]
    async fn a_lagging_subscriber_gets_a_gap_from_its_last_durable_event() {
        let (outgoing, _incoming) = mpsc::channel(1);
        let shared = Shared::new(outgoing, 1);
        let mut notifications = Notifications {
            receiver: shared.notifications.subscribe(),
            after: 0,
        };
        shared.deliver(
            json!({"jsonrpc":"2.0","method":"event","params":{"durable":true,"sequence":3}}),
        );
        notifications.recv().await.unwrap();
        for sequence in 0..NOTIFICATION_BUFFER as u64 + 5 {
            shared.deliver(json!({"jsonrpc":"2.0","method":"event","params":{"durable":true,"sequence":10 + sequence}}));
        }
        let gap = notifications.recv().await.unwrap();
        assert_eq!(gap["method"], "events.gap");
        assert_eq!(gap["params"]["after"], 3);
        assert_eq!(gap["params"]["skipped"], 5);
        assert_eq!(gap["params"]["recover_with"], "events.after");
    }
}
