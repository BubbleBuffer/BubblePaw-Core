//! The tunnel as a host-API carrier: one JSON-RPC frame per tunnel message,
//! empty messages as keepalives. Host side, a tunnel feeds the shared session
//! loop (in process) or a control-endpoint session (attached); client side, it
//! carries a [`HostClient`]. No method logic lives here.
use super::KEEPALIVE;
#[cfg(feature = "host")]
use crate::host_api::{HostCtx, ReadScope};
use crate::host_client::{CarrierEnds, HostClient};
#[cfg(feature = "host")]
use crate::host_transport::{self, HOST_PROTOCOL_VERSION};
#[cfg(feature = "host")]
use crate::profile::Profile;
#[cfg(feature = "host")]
use crate::rpc_session::{self, Incoming, RECORD_LIMIT};
#[cfg(feature = "host")]
use anyhow::{Context, Result, anyhow, bail};
use bubblepaw_core::CancellationToken;
use bubblepaw_remote::frame::{FrameReader, FrameWriter};
use bubblepaw_remote::tunnel::{TunnelReader, TunnelWriter};
use serde_json::Value;
#[cfg(feature = "host")]
use serde_json::json;
#[cfg(feature = "host")]
use std::path::Path;
use std::time::Duration;
#[cfg(feature = "host")]
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// How long a stalled peer may hold one outgoing message.
pub(super) const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long an attached session may take to open on the local host.
#[cfg(feature = "host")]
const ATTACH_WAIT: Duration = Duration::from_secs(5);

fn keepalive() -> tokio::time::Interval {
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + KEEPALIVE, KEEPALIVE);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval
}

/// Send each queued message, and a keepalive when the tunnel has been quiet.
/// Ends when the queue closes, `stop` fires, or a send fails.
pub(super) async fn write_loop<W: FrameWriter>(
    mut writer: TunnelWriter<W>,
    mut queue: mpsc::Receiver<Vec<u8>>,
    stop: CancellationToken,
) {
    let mut keepalive = keepalive();
    loop {
        let message = tokio::select! {
            _ = stop.cancelled() => break,
            message = queue.recv() => match message { Some(message) => message, None => break },
            _ = keepalive.tick() => Vec::new(),
        };
        match tokio::time::timeout(WRITE_TIMEOUT, writer.send(&message)).await {
            Ok(Ok(())) => keepalive.reset(),
            _ => break,
        }
    }
}

/// The next non-keepalive message, `None` at the end of the tunnel.
pub(super) async fn next_message<R: FrameReader>(reader: &mut TunnelReader<R>) -> Option<Vec<u8>> {
    loop {
        match reader.recv().await {
            Ok(message) if message.is_empty() => continue,
            Ok(message) => return Some(message),
            Err(_) => return None,
        }
    }
}

#[cfg(feature = "host")]
/// Serve one admitted tunnel in this process through the shared session loop,
/// with `profile` and host-wide reads, until either side ends it or `stop` fires.
pub(super) async fn serve_in_process<R, W>(
    ctx: &HostCtx,
    profile: Profile,
    mut reader: TunnelReader<R>,
    writer: TunnelWriter<W>,
    stop: CancellationToken,
) -> Result<()>
where
    R: FrameReader + 'static,
    W: FrameWriter + 'static,
{
    let mut ctx = ctx.clone();
    ctx.read_scope = ReadScope::Host;
    ctx.known_workspaces_only = true;
    let (input, incoming) = mpsc::channel::<Incoming>(16);
    let (output, mut replies) = mpsc::channel::<Value>(128);
    let (queue, queued) = mpsc::channel::<Vec<u8>>(16);
    let read_task = tokio::spawn(async move {
        while let Some(message) = next_message(&mut reader).await {
            let record = if message.len() >= RECORD_LIMIT {
                Err(anyhow!("record exceeds 1 MiB"))
            } else {
                Ok(Some(message))
            };
            let terminal = record.is_err();
            if input.send(record).await.is_err() || terminal {
                return;
            }
        }
        let _ = input.send(Ok(None)).await;
    });
    let encode_task = tokio::spawn(async move {
        while let Some(reply) = replies.recv().await {
            let Ok(bytes) = serde_json::to_vec(&reply) else {
                continue;
            };
            if queue.send(bytes).await.is_err() {
                break;
            }
        }
    });
    let write_task = tokio::spawn(write_loop(writer, queued, stop.clone()));
    let result = rpc_session::serve(
        &ctx,
        profile,
        incoming,
        output,
        &stop,
        std::future::pending::<()>,
    )
    .await;
    read_task.abort();
    let _ = read_task.await;
    // `serve` dropped its output: queued replies drain, then the writer ends.
    let _ = encode_task.await;
    if tokio::time::timeout(WRITE_TIMEOUT, write_task)
        .await
        .is_err()
    {
        stop.cancel();
    }
    result
}

#[cfg(feature = "host")]
/// Serve one admitted tunnel through the host that owns `data_dir`, on an
/// `attach.session` with `profile`. Frames pass through unchanged, except that
/// a newline inside a message (only JSON whitespace) becomes a space.
pub(super) async fn serve_attached<R, W>(
    data_dir: &Path,
    profile: Profile,
    mut reader: TunnelReader<R>,
    writer: TunnelWriter<W>,
    stop: CancellationToken,
) -> Result<()>
where
    R: FrameReader + 'static,
    W: FrameWriter + 'static,
{
    let connection = host_transport::connect(data_dir, ATTACH_WAIT)
        .await
        .context("the host for this data directory is not reachable")?;
    let (read, mut write) = tokio::io::split(connection);
    let mut lines = BufReader::new(read);
    let open =
        json!({"jsonrpc":"2.0","id":0,"method":"attach.session","params":{"profile":profile}});
    let mut frame = serde_json::to_vec(&open)?;
    frame.push(b'\n');
    write.write_all(&frame).await?;
    write.flush().await?;
    let opened = rpc_session::read_record_async(&mut lines)
        .await?
        .context("the host closed before the session opened")?;
    let opened: Value = serde_json::from_slice(&opened).context("unreadable session reply")?;
    if let Some(error) = opened.get("error") {
        bail!("the host refused the session: {}", error["message"]);
    }
    if opened["result"]["protocol_version"] != HOST_PROTOCOL_VERSION {
        bail!(
            "the host speaks protocol version {}, this one {HOST_PROTOCOL_VERSION}; restart the older one",
            opened["result"]["protocol_version"]
        );
    }
    let (queue, queued) = mpsc::channel::<Vec<u8>>(16);
    let write_task = tokio::spawn(write_loop(writer, queued, stop.clone()));
    let to_host = tokio::spawn(async move {
        while let Some(mut message) = next_message(&mut reader).await {
            if message.len() >= RECORD_LIMIT {
                break;
            }
            for byte in &mut message {
                if *byte == b'\n' {
                    *byte = b' ';
                }
            }
            message.push(b'\n');
            if write.write_all(&message).await.is_err() || write.flush().await.is_err() {
                break;
            }
        }
        // End of input ends the host's session loop.
        let _ = write.shutdown().await;
    });
    let from_host = async {
        while let Ok(Some(mut record)) = rpc_session::read_record_async(&mut lines).await {
            while record
                .last()
                .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
            {
                record.pop();
            }
            if queue.send(record).await.is_err() {
                break;
            }
        }
    };
    tokio::select! {
        _ = stop.cancelled() => {}
        _ = from_host => {}
    }
    to_host.abort();
    let _ = to_host.await;
    drop(queue);
    if tokio::time::timeout(WRITE_TIMEOUT, write_task)
        .await
        .is_err()
    {
        stop.cancel();
    }
    Ok(())
}

/// A host-API client over an admitted tunnel to a remote host.
pub(super) fn client<R, W>(mut reader: TunnelReader<R>, writer: TunnelWriter<W>) -> HostClient
where
    R: FrameReader + 'static,
    W: FrameWriter + 'static,
{
    HostClient::over_carrier(move |ends: CarrierEnds| {
        let CarrierEnds {
            mut records,
            frames,
            stop,
        } = ends;
        let (queue, queued) = mpsc::channel::<Vec<u8>>(16);
        let encode = {
            let stop = stop.clone();
            tokio::spawn(async move {
                loop {
                    let record = tokio::select! {
                        _ = stop.cancelled() => break,
                        record = records.recv() => record,
                    };
                    let Some(Ok(Some(bytes))) = record else { break };
                    if queue.send(bytes).await.is_err() {
                        break;
                    }
                }
            })
        };
        let write = tokio::spawn(write_loop(writer, queued, stop.clone()));
        let read = tokio::spawn(async move {
            loop {
                let message = tokio::select! {
                    _ = stop.cancelled() => break,
                    message = next_message(&mut reader) => message,
                };
                let Some(message) = message else { break };
                // A frame this client cannot read is dropped; replies it was
                // waiting for then fail as disconnected when the session ends.
                if let Ok(frame) = serde_json::from_slice::<Value>(&message)
                    && frames.send(frame).await.is_err()
                {
                    break;
                }
            }
            // Dropping `frames` ends the client's session.
        });
        vec![encode, write, read]
    })
}
