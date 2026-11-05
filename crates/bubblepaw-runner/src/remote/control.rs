//! The remote server's local control endpoint, so `bubblepaw remote pair` and
//! `remote devices revoke` reach a `remote serve` running in another process.
//! It listens like the host's own control endpoint (a socket or pipe for the
//! current user only) in `RemotePaths::control` and speaks JSON lines: one
//! request, then events, with an `approve` or `decline` answer while pairing.
use super::cli::ProfileArg;
use super::server::Inner;
use crate::host_transport::{self, Connection, HostEndpoint};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::io::{ReadHalf, WriteHalf};

/// Longest line either side accepts.
const LINE_LIMIT: u64 = 64 * 1024;
/// How long the user may take to answer a pairing request.
const ANSWER_WAIT: Duration = Duration::from_secs(300);
/// How long a client waits for the endpoint to accept it.
const CONNECT_WAIT: Duration = Duration::from_secs(2);

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub(super) enum Request {
    State,
    Pair {
        profile: ProfileArg,
        /// Also grant the device model-traffic forwarding through this host.
        #[serde(default)]
        egress: bool,
    },
    Approve,
    Decline,
    Revoke {
        id: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "lowercase")]
pub(super) enum Event {
    State {
        state: super::ShareState,
    },
    /// The offer to show: the QR payload, its code and its lifetime in seconds.
    Offer {
        payload: String,
        code: String,
        expires_in: u64,
    },
    /// A device wants to pair; answer with `approve` or `decline`.
    Request {
        device_name: String,
        device_id: String,
        comparison: String,
    },
    /// The offer expired without a device.
    Expired,
    Paired {
        id: String,
        label: String,
        profile: String,
        #[serde(default)]
        egress: bool,
    },
    Declined,
    Revoked {
        removed: bool,
    },
    Error {
        message: String,
    },
}

/// Accept control clients until the server stops.
pub(super) async fn serve(inner: Arc<Inner>, mut endpoint: HostEndpoint) {
    loop {
        let accepted = tokio::select! {
            _ = inner.stop.cancelled() => return,
            accepted = endpoint.accept() => accepted,
        };
        match accepted {
            Ok(connection) => {
                let inner = inner.clone();
                tokio::spawn(async move {
                    if let Err(error) = client(inner, connection).await {
                        tracing::debug!("remote control client ended: {error:#}");
                    }
                });
            }
            Err(error) => {
                tracing::warn!("remote control accept failed: {error:#}");
                tokio::select! {
                    _ = inner.stop.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
            }
        }
    }
}

async fn client(inner: Arc<Inner>, connection: Connection) -> Result<()> {
    let mut channel = Channel::new(connection);
    let request = channel.read::<Request>().await?.context("no request")?;
    match request {
        Request::State => {
            channel
                .write(&Event::State {
                    state: inner.share_state(),
                })
                .await
        }
        Request::Pair { profile, egress } => pair(&inner, &mut channel, profile, egress).await,
        Request::Revoke { id } => {
            let event = match inner.revoke(&id).await {
                Ok(removed) => Event::Revoked { removed },
                Err(error) => error_event(error),
            };
            channel.write(&event).await
        }
        Request::Approve | Request::Decline => {
            channel
                .write(&Event::Error {
                    message: "no pairing request to answer".to_owned(),
                })
                .await
        }
    }
}

pub(crate) async fn share_state(paths: &super::RemotePaths) -> Result<Option<super::ShareState>> {
    let Some(mut channel) = Channel::connect(&paths.control).await? else {
        return Ok(None);
    };
    tokio::time::timeout(CONNECT_WAIT, async {
        channel.write(&Request::State).await?;
        match channel.read::<Event>().await? {
            Some(Event::State { state }) => Ok(Some(state)),
            _ => bail!("unexpected remote share state reply"),
        }
    })
    .await
    .context("remote share state timed out")?
}

fn error_event(error: anyhow::Error) -> Event {
    Event::Error {
        message: format!("{error:#}"),
    }
}

async fn pair(
    inner: &Inner,
    channel: &mut Channel,
    profile: ProfileArg,
    egress: bool,
) -> Result<()> {
    let mut offer = match inner.pair(profile.into(), egress).await {
        Ok(offer) => offer,
        Err(error) => return channel.write(&error_event(error)).await,
    };
    channel
        .write(&Event::Offer {
            payload: offer.qr_payload().encode(),
            code: offer.code().to_owned(),
            expires_in: offer.expires_in().as_secs(),
        })
        .await?;
    // A client that hangs up (or says anything) before a device arrives
    // withdraws the offer.
    let request = tokio::select! {
        request = offer.next() => request,
        _ = inner.stop.cancelled() => None,
        _ = channel.read::<Request>() => return Ok(()),
    };
    let Some(request) = request else {
        return channel.write(&Event::Expired).await;
    };
    channel
        .write(&Event::Request {
            device_name: request.device_name().to_owned(),
            device_id: request.device_id().to_owned(),
            comparison: request.comparison().to_owned(),
        })
        .await?;
    let answer = tokio::select! {
        answer = tokio::time::timeout(ANSWER_WAIT, channel.read::<Request>()) => answer,
        _ = inner.stop.cancelled() => Ok(Ok(None)),
    };
    match answer {
        Ok(Ok(Some(Request::Approve))) => {
            let event = match request.approve(None).await {
                Ok(device) => Event::Paired {
                    id: device.id,
                    label: device.label,
                    profile: serde_json::to_value(device.profile)?
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                    egress: device.egress,
                },
                Err(error) => error_event(error),
            };
            channel.write(&event).await
        }
        _ => {
            request.decline();
            // The client may be gone; the decline stands either way.
            let _ = channel.write(&Event::Declined).await;
            Ok(())
        }
    }
}

/// One JSON-lines connection.
pub(super) struct Channel {
    reader: BufReader<ReadHalf<Connection>>,
    writer: WriteHalf<Connection>,
}

impl Channel {
    fn new(connection: Connection) -> Self {
        let (reader, writer) = tokio::io::split(connection);
        Self {
            reader: BufReader::new(reader),
            writer,
        }
    }

    /// Connect to the control endpoint of a `remote serve` running for `dir`;
    /// `None` when none is running.
    pub(super) async fn connect(dir: &Path) -> Result<Option<Self>> {
        if host_transport::read_host_metadata(dir)?.is_none() {
            return Ok(None);
        }
        // A file left by a server that crashed names an endpoint nobody serves.
        match host_transport::connect(dir, CONNECT_WAIT).await {
            Ok(connection) => Ok(Some(Self::new(connection))),
            Err(_) => Ok(None),
        }
    }

    pub(super) async fn write<T: Serialize>(&mut self, message: &T) -> Result<()> {
        let mut line = serde_json::to_vec(message)?;
        line.push(b'\n');
        self.writer.write_all(&line).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// The next message; `None` when the other side closed the connection.
    pub(super) async fn read<T: for<'de> Deserialize<'de>>(&mut self) -> Result<Option<T>> {
        read_line(&mut self.reader).await
    }
}

async fn read_line<R, T>(reader: &mut BufReader<R>) -> Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let mut line = String::new();
    let read = reader.take(LINE_LIMIT).read_line(&mut line).await?;
    if read == 0 {
        return Ok(None);
    }
    if !line.ends_with('\n') {
        bail!("control line too long or cut off");
    }
    Ok(Some(
        serde_json::from_str(&line).context("unreadable control message")?,
    ))
}
