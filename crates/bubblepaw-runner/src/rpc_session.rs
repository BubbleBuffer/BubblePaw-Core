//! One host-API JSON-RPC session over newline-delimited frames: the framing,
//! the event forwarder and the request loop. Stdio and the host control
//! endpoint both run this loop; neither adds methods of its own, and every
//! request is answered by the shared table through `rpc::handle_bytes`.
use crate::host_api::{HostCtx, Profile, events, rpc};
use anyhow::{Context, Result, ensure};
use bubblepaw_core::{CancellationToken, HarnessEvent};
use serde_json::{Value, json};
use std::future::Future;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

/// The largest frame either side accepts, newline included.
pub const RECORD_LIMIT: usize = 1024 * 1024;

/// How long the endpoint writer may wait for a client to take one frame.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// One framed input: a record, the end of input, or a framing error.
pub type Incoming = Result<Option<Vec<u8>>>;

/// Run one session until input ends, a valid `shutdown` request is answered,
/// `interrupt` resolves while waiting for input, or `cancellation` fires.
///
/// Records arrive on `incoming`; replies and notifications (`event`, and
/// `events.gap` when the forwarder lags) leave on `output`.
pub async fn serve<F, Fut>(
    ctx: &HostCtx,
    profile: Profile,
    mut incoming: mpsc::Receiver<Incoming>,
    output: mpsc::Sender<Value>,
    cancellation: &CancellationToken,
    interrupt: F,
) -> Result<()>
where
    F: Fn() -> Fut,
    Fut: Future,
{
    let event_forwarder = forward_events(ctx, output.clone())?;
    let loop_result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => Ok(()),
        result = async {
        loop {
            let record = tokio::select! {
                _ = cancellation.cancelled() => break,
                record = incoming.recv() => match record { Some(record) => record?, None => break },
                _ = interrupt() => break,
            };
            let Some(record) = record else { break; };
            let quit = is_shutdown(&record);
            let response = tokio::select! {
                _ = cancellation.cancelled() => break,
                response = rpc::handle_bytes(ctx, profile, &record) => response,
            };
            if let Some(response) = response {
                output.send(response).await?;
            }
            // After the reply, end the session. The host owns shared shutdown.
            if quit { break; }
        }
        Ok::<_, anyhow::Error>(())
        } => result,
    };
    drop(incoming);
    event_forwarder.abort();
    let _ = event_forwarder.await;
    loop_result
}

/// Run one session over an async byte stream, such as an endpoint connection.
/// `reader` must be the reader that any earlier frames were read through, so
/// bytes it already buffered are not lost.
pub async fn serve_stream<R, W>(
    ctx: &HostCtx,
    profile: Profile,
    reader: R,
    mut writer: W,
    cancellation: &CancellationToken,
) -> Result<()>
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (output, mut records) = mpsc::channel::<Value>(128);
    // A client that neither reads nor disconnects ends its own session.
    let writer_task = tokio::spawn(async move {
        while let Some(record) = records.recv().await {
            let mut bytes = serde_json::to_vec(&record)?;
            bytes.push(b'\n');
            tokio::time::timeout(WRITE_TIMEOUT, async {
                writer.write_all(&bytes).await?;
                writer.flush().await
            })
            .await
            .context("session write timed out")??;
        }
        Ok::<_, anyhow::Error>(())
    });
    let (input, incoming) = mpsc::channel(16);
    let reader_task = tokio::spawn(async move {
        let mut reader = reader;
        loop {
            let record = read_record_async(&mut reader).await;
            let terminal = !matches!(&record, Ok(Some(_)));
            if input.send(record).await.is_err() || terminal {
                break;
            }
        }
    });
    let result = serve(
        ctx,
        profile,
        incoming,
        output,
        cancellation,
        std::future::pending::<()>,
    )
    .await;
    reader_task.abort();
    let _ = reader_task.await;
    // `serve` dropped its sender: the writer drains what is queued and stops.
    let _ = writer_task.await;
    result
}

/// Forward visible harness events as `event` notifications. A full output queue
/// drops events and reports them once with `events.gap`, naming the last durable
/// sequence forwarded, so the client recovers with `events.after`.
fn forward_events(
    ctx: &HostCtx,
    notifications: mpsc::Sender<Value>,
) -> Result<tokio::task::JoinHandle<()>> {
    let subscription = events::subscribe(ctx, None, None)?;
    let mut receiver = subscription.receiver;
    // The last durable sequence forwarded; a gap is recovered with events.after from it.
    let mut forwarded = subscription.last_seq;
    let forward_ctx = ctx.clone();
    let live = ctx.live_config.clone();
    let mut statuses = live.as_ref().map(|live| live.mcp().subscribe());
    Ok(tokio::spawn(async move {
        let mut skipped = 0u64;
        loop {
            let received = tokio::select! {
                received = receiver.recv() => received,
                changed = mcp_status(&mut statuses) => {
                    // `mcp.server` carries one server's whole state; after a lag
                    // every server's current state is sent again.
                    let changed = match changed {
                        Some(Ok(status)) => vec![status],
                        Some(Err(())) => live.as_ref().map(|live| live.mcp().servers()).unwrap_or_default(),
                        None => { statuses = None; Vec::new() }
                    };
                    for status in changed {
                        let _ = notifications.try_send(json!({"jsonrpc":"2.0","method":"mcp.server","params":status}));
                    }
                    continue;
                }
            };
            let event: HarnessEvent = match received {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                    skipped += count;
                    continue;
                }
                Err(_) => break,
            };
            if !events::visible(&forward_ctx, &event, None) {
                if event.durable && skipped == 0 {
                    forwarded = forwarded.max(event.sequence);
                }
                continue;
            }
            if skipped > 0 {
                if notifications.try_send(json!({"jsonrpc":"2.0","method":"events.gap","params":{"skipped":skipped,"after":forwarded,"recover_with":"events.after"}})).is_err() { skipped += 1; continue; }
                skipped = 0;
            }
            let (durable, sequence) = (event.durable, event.sequence);
            if notifications
                .try_send(json!({"jsonrpc":"2.0","method":"event","params":event}))
                .is_err()
            {
                skipped += 1;
            } else if durable {
                forwarded = forwarded.max(sequence);
            }
        }
    }))
}

/// The next MCP server status change: `Err` after a lag, `None` once the host
/// is gone. Without an MCP host this never resolves.
async fn mcp_status(
    statuses: &mut Option<tokio::sync::broadcast::Receiver<bubblepaw_mcp::McpServerStatus>>,
) -> Option<Result<bubblepaw_mcp::McpServerStatus, ()>> {
    let Some(receiver) = statuses else {
        return std::future::pending().await;
    };
    match receiver.recv().await {
        Ok(status) => Some(Ok(status)),
        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => Some(Err(())),
        Err(tokio::sync::broadcast::error::RecvError::Closed) => None,
    }
}

/// A valid request naming `shutdown` ends the loop once its reply is written.
/// The envelope checks match `rpc::handle`, so an invalid record never quits.
fn is_shutdown(record: &[u8]) -> bool {
    let Ok(request) = serde_json::from_slice::<Value>(record) else {
        return false;
    };
    let id_valid = request
        .get("id")
        .is_none_or(|id| id.is_string() || id.is_number() || id.is_null());
    request["jsonrpc"] == "2.0" && request["method"].as_str() == Some("shutdown") && id_valid
}

/// Move bytes from `buffer` into `record` up to and including a newline.
/// Returns how many bytes were taken and whether the record is complete.
fn take_line(record: &mut Vec<u8>, buffer: &[u8]) -> Result<(usize, bool)> {
    let end = buffer
        .iter()
        .position(|b| *b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(buffer.len());
    ensure!(
        record.len() + end <= RECORD_LIMIT,
        "protocol record exceeds 1 MiB"
    );
    record.extend_from_slice(&buffer[..end]);
    Ok((end, buffer[end - 1] == b'\n'))
}

/// Read one newline-terminated record from blocking input. A final record
/// without a newline is returned as is; `None` is the end of input.
pub fn read_record(reader: &mut impl std::io::BufRead) -> Result<Option<Vec<u8>>> {
    let mut record = Vec::new();
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok((!record.is_empty()).then_some(record));
        }
        let (taken, complete) = take_line(&mut record, buffer)?;
        reader.consume(taken);
        if complete {
            return Ok(Some(record));
        }
    }
}

/// [`read_record`] for async input, with the same framing and limit.
pub async fn read_record_async(
    reader: &mut (impl AsyncBufRead + Unpin),
) -> Result<Option<Vec<u8>>> {
    let mut record = Vec::new();
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            return Ok((!record.is_empty()).then_some(record));
        }
        let (taken, complete) = take_line(&mut record, buffer)?;
        reader.consume(taken);
        if complete {
            return Ok(Some(record));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RECORD_LIMIT, is_shutdown, read_record, read_record_async};

    #[test]
    fn shutdown_quits_only_for_a_valid_request_envelope() {
        assert!(is_shutdown(
            br#"{"jsonrpc":"2.0","id":1,"method":"shutdown"}"#
        ));
        assert!(is_shutdown(br#"{"jsonrpc":"2.0","method":"shutdown"}"#));
        assert!(!is_shutdown(br#"{"jsonrpc":"wrong","method":"shutdown"}"#));
        assert!(!is_shutdown(
            br#"{"jsonrpc":"2.0","id":{},"method":"shutdown"}"#
        ));
        assert!(!is_shutdown(br#"{"jsonrpc":"2.0","id":1,"method":7}"#));
        assert!(!is_shutdown(br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#));
        assert!(!is_shutdown(b"not json"));
    }

    #[tokio::test]
    async fn blocking_and_async_framing_agree() {
        let input = b"one\ntwo\nlast".to_vec();
        let mut sync = std::io::BufReader::with_capacity(3, &input[..]);
        let mut asynchronous = tokio::io::BufReader::with_capacity(3, &input[..]);
        for expected in [&b"one\n"[..], b"two\n", b"last"] {
            assert_eq!(read_record(&mut sync).unwrap().unwrap(), expected);
            assert_eq!(
                read_record_async(&mut asynchronous).await.unwrap().unwrap(),
                expected
            );
        }
        assert!(read_record(&mut sync).unwrap().is_none());
        assert!(
            read_record_async(&mut asynchronous)
                .await
                .unwrap()
                .is_none()
        );

        let large = vec![b'x'; RECORD_LIMIT + 1];
        assert!(read_record(&mut &large[..]).is_err());
        assert!(read_record_async(&mut &large[..]).await.is_err());
    }
}
