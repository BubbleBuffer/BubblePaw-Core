//! Host protocol primitives: the protocol record reader and writer and the Ctrl-C listener.
use super::*;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

const RECORD_LIMIT: u64 = 1024 * 1024;

pub(super) async fn read_json<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<Option<Value>> {
    let mut bytes = Vec::new();
    let size = reader
        .take(RECORD_LIMIT + 1)
        .read_until(b'\n', &mut bytes)
        .await?;
    if size == 0 {
        return Ok(None);
    }
    ensure!(size as u64 <= RECORD_LIMIT, "protocol record exceeds 1 MiB");
    ensure!(bytes.last() == Some(&b'\n'), "interrupted protocol record");
    Ok(Some(serde_json::from_slice(&bytes)?))
}

pub(super) async fn write_json<W: AsyncWrite + Unpin>(
    writer: &mut W,
    record: &Value,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(record)?;
    ensure!(
        bytes.len() < RECORD_LIMIT as usize,
        "protocol record exceeds 1 MiB"
    );
    bytes.push(b'\n');
    tokio::time::timeout(Duration::from_secs(5), async {
        writer.write_all(&bytes).await?;
        writer.flush().await
    })
    .await
    .context("host protocol write timed out")??;
    Ok(())
}

/// The Ctrl-C listener. Both variants register at creation and buffer a signal
/// delivered before the first poll.
#[cfg(unix)]
pub(super) type Interrupt = tokio::signal::unix::Signal;
#[cfg(windows)]
pub(super) type Interrupt = tokio::signal::windows::CtrlC;

#[cfg(unix)]
pub(super) fn interrupt_listener() -> std::io::Result<Interrupt> {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
}
#[cfg(windows)]
pub(super) fn interrupt_listener() -> std::io::Result<Interrupt> {
    tokio::signal::windows::ctrl_c()
}
