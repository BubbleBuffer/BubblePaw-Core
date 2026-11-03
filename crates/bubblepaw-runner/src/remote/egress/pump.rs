//! Copy bytes between a TCP stream and an admitted tunnel, in both directions.
//! Both ends of an egress session use it: the forwarding device with its
//! upstream socket, the requesting device with the local proxy's client.
//!
//! One tunnel message is one chunk: a tag byte, then up to [`CHUNK`] bytes. An
//! empty message is a keepalive. Bytes cross untouched; TLS runs end to end
//! between the model client and the provider, so this code never sees plaintext.
use super::super::adapter::{WRITE_TIMEOUT, next_message, write_loop};
use bubblepaw_core::CancellationToken;
use bubblepaw_remote::frame::{FrameReader, FrameWriter};
use bubblepaw_remote::tunnel::{TunnelReader, TunnelWriter};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

/// Largest payload of one data message.
pub(super) const CHUNK: usize = 16 * 1024;
/// A data message: the rest is stream bytes.
pub(super) const TAG_DATA: u8 = 1;
/// The sender will send nothing more (its end of the stream closed).
pub(super) const TAG_END: u8 = 2;
/// After this side's stream ends, how long the other direction may still run.
const HALF_CLOSE_GRACE: Duration = Duration::from_secs(10);

/// Run until the stream and the tunnel have both finished, or `stop` fires.
pub(super) async fn pump<S, R, W>(
    stream: S,
    mut reader: TunnelReader<R>,
    writer: TunnelWriter<W>,
    stop: CancellationToken,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    R: FrameReader + 'static,
    W: FrameWriter + 'static,
{
    let (mut stream_in, mut stream_out) = tokio::io::split(stream);
    let (queue, queued) = mpsc::channel::<Vec<u8>>(8);
    let write_task = tokio::spawn(write_loop(writer, queued, stop.clone()));
    {
        // The stream's bytes, towards the other device.
        let up = async {
            let mut buffer = vec![0u8; CHUNK + 1];
            buffer[0] = TAG_DATA;
            loop {
                let read = tokio::select! {
                    _ = stop.cancelled() => return,
                    read = stream_in.read(&mut buffer[1..]) => read,
                };
                match read {
                    Ok(read) if read > 0 => {
                        if queue.send(buffer[..=read].to_vec()).await.is_err() {
                            return;
                        }
                    }
                    _ => {
                        let _ = queue.send(vec![TAG_END]).await;
                        return;
                    }
                }
            }
        };
        // The other device's bytes, towards the stream.
        let down = async {
            loop {
                let message = tokio::select! {
                    _ = stop.cancelled() => return,
                    message = next_message(&mut reader) => message,
                };
                match message.as_deref() {
                    Some([TAG_DATA, data @ ..]) if !data.is_empty() => {
                        let written = tokio::select! {
                            _ = stop.cancelled() => return,
                            written = tokio::time::timeout(WRITE_TIMEOUT, stream_out.write_all(data)) => written,
                        };
                        if !matches!(written, Ok(Ok(()))) {
                            return;
                        }
                    }
                    Some([TAG_END]) => {
                        let _ = stream_out.shutdown().await;
                        return;
                    }
                    _ => return,
                }
            }
        };
        tokio::pin!(up);
        tokio::pin!(down);
        tokio::select! {
            _ = &mut down => {}
            _ = &mut up => {
                // The stream ended (a clean close from the client or the
                // provider); the answer may still be on its way.
                let _ = tokio::time::timeout(HALF_CLOSE_GRACE, &mut down).await;
            }
        }
    }
    // Let the end marker leave before the tunnel closes.
    drop(queue);
    if tokio::time::timeout(WRITE_TIMEOUT, write_task)
        .await
        .is_err()
    {
        stop.cancel();
    }
}
