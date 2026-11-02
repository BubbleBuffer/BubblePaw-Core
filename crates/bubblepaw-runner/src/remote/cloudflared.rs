//! The managed `cloudflared`: started with the server, its address read from
//! its log, restarted with backoff when it exits, and killed with the server.
//! It is the one external program in remote serving, and it only ever sees the
//! tunnel's ciphertext.
use super::RemoteAddress;
use super::domain;
use super::reach::{CloudflareTunnel, TunnelMode};
use super::server::{BACKOFF_MAX, BACKOFF_MIN, Inner, RemoteStatus};
use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

/// How long `cloudflared` may take to publish the endpoint.
const PUBLISH_WAIT: Duration = Duration::from_secs(90);
/// A run that lasted this long resets the restart backoff.
const STABLE_RUN: Duration = Duration::from_secs(60);
/// Longest log line kept; the rest of a longer line is dropped.
const LINE_LIMIT: usize = 8 * 1024;
/// What `cloudflared` logs once a named tunnel connection is up.
const REGISTERED: &str = "registered tunnel connection";

/// The program's file name on this platform.
pub fn program_name() -> &'static str {
    if cfg!(windows) {
        "cloudflared.exe"
    } else {
        "cloudflared"
    }
}

/// Find `cloudflared`: the configured path, else the copy installed by
/// `bubblepaw remote tunnel install` in `installed_dir`, else `PATH`.
pub fn find(configured: Option<&Path>, installed_dir: &Path) -> Result<PathBuf> {
    if let Some(path) = configured {
        if path.is_file() {
            return Ok(path.to_owned());
        }
        bail!(
            "remote.cloudflared names {}, which does not exist",
            path.display()
        );
    }
    let installed = installed_dir.join(program_name());
    if installed.is_file() {
        return Ok(installed);
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(program_name());
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    bail!(
        "cloudflared was not found; run `bubblepaw remote tunnel install`, put it on PATH, \
         or set remote.cloudflared to its path"
    )
}

/// The trycloudflare host in a quick tunnel's log line, if it has one.
pub(super) fn quick_host(line: &str) -> Option<String> {
    let mut rest = line;
    while let Some(start) = rest.find("https://") {
        let tail = &rest[start + "https://".len()..];
        let host: String = tail
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.')
            .collect();
        let host = host.to_ascii_lowercase();
        if host.ends_with(".trycloudflare.com") && host != "api.trycloudflare.com" {
            return Some(host);
        }
        rest = tail;
    }
    None
}

/// Keep `cloudflared` running until the server stops.
pub(super) async fn supervise(inner: Arc<Inner>, tunnel: CloudflareTunnel, port: u16) {
    let mut backoff = BACKOFF_MIN;
    loop {
        inner.status.send_replace(RemoteStatus::Connecting);
        let started = Instant::now();
        let result = run_once(&inner, &tunnel, port).await;
        if inner.stop.is_cancelled() {
            return;
        }
        if tunnel.mode == TunnelMode::Quick {
            // The next start gets a new address; the old one is gone.
            inner.address.send_replace(None);
        }
        let error = match result {
            Ok(()) => "cloudflared exited".to_owned(),
            Err(error) => format!("{error:#}"),
        };
        inner.set_offline(Some(format!("{error}; restarting in {backoff:?}")));
        if started.elapsed() >= STABLE_RUN {
            backoff = BACKOFF_MIN;
        }
        tokio::select! {
            _ = inner.stop.cancelled() => return,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// The arguments for one run, and the fixed address of a named tunnel.
async fn prepare(
    tunnel: &CloudflareTunnel,
    port: u16,
) -> Result<(Vec<OsString>, Option<RemoteAddress>)> {
    let origin = format!("http://127.0.0.1:{port}");
    let mut args: Vec<OsString> = vec!["--no-autoupdate".into(), "tunnel".into()];
    match &tunnel.mode {
        TunnelMode::Quick => {
            args.extend(["--url".into(), origin.into()]);
            Ok((args, None))
        }
        TunnelMode::Domain { hostname } => {
            let named = domain::prepare(tunnel, hostname).await?;
            args.extend([
                "run".into(),
                "--credentials-file".into(),
                named.credentials.into_os_string(),
                "--url".into(),
                origin.into(),
                named.tunnel_id.into(),
            ]);
            Ok((
                args,
                Some(RemoteAddress::Direct(format!("wss://{hostname}"))),
            ))
        }
    }
}

async fn run_once(inner: &Inner, tunnel: &CloudflareTunnel, port: u16) -> Result<()> {
    let (args, fixed) = tokio::select! {
        _ = inner.stop.cancelled() => return Ok(()),
        prepared = prepare(tunnel, port) => prepared?,
    };
    let mut command = Command::new(&tunnel.cloudflared);
    command
        .args(&args)
        // Held open until the run ends; nothing is ever written to it.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("could not start {}", tunnel.cloudflared.display()))?;
    let _stdin = child.stdin.take();
    let (lines_tx, mut lines) = mpsc::channel::<String>(64);
    if let Some(out) = child.stdout.take() {
        tokio::spawn(forward_lines(out, lines_tx.clone()));
    }
    if let Some(err) = child.stderr.take() {
        tokio::spawn(forward_lines(err, lines_tx));
    } else {
        drop(lines_tx);
    }
    let publish_deadline = tokio::time::Instant::now() + PUBLISH_WAIT;
    let mut published = false;
    let mut lines_open = true;
    let result = loop {
        tokio::select! {
            _ = inner.stop.cancelled() => break Ok(()),
            status = child.wait() => {
                break match status {
                    Ok(status) => Err(anyhow::anyhow!("cloudflared exited ({status})")),
                    Err(error) => Err(anyhow::anyhow!("cloudflared failed: {error}")),
                };
            }
            line = lines.recv(), if lines_open => {
                let Some(line) = line else {
                    lines_open = false;
                    continue;
                };
                tracing::debug!(target: "bubblepaw::cloudflared", "{line}");
                if published {
                    continue;
                }
                let address = match &tunnel.mode {
                    TunnelMode::Quick => quick_host(&line)
                        .map(|host| RemoteAddress::Direct(format!("wss://{host}"))),
                    TunnelMode::Domain { .. } => line
                        .to_ascii_lowercase()
                        .contains(REGISTERED)
                        .then(|| fixed.clone())
                        .flatten(),
                };
                if let Some(address) = address {
                    published = true;
                    *inner.last_error.lock().unwrap() = None;
                    inner.address.send_replace(Some(address));
                    inner.status.send_replace(RemoteStatus::Online);
                }
            }
            _ = tokio::time::sleep_until(publish_deadline), if !published => {
                break Err(anyhow::anyhow!(
                    "cloudflared did not publish the endpoint within {PUBLISH_WAIT:?}"
                ));
            }
        }
    };
    let _ = child.kill().await;
    result
}

/// Send each line of `stream` to `lines`, cut to [`LINE_LIMIT`].
async fn forward_lines(stream: impl AsyncRead + Unpin, lines: mpsc::Sender<String>) {
    let mut reader = BufReader::new(stream);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        // Read one line in bounded pieces.
        loop {
            let chunk = match reader.fill_buf().await {
                Ok([]) => return finish(&buffer, &lines).await,
                Ok(chunk) => chunk,
                Err(_) => return,
            };
            let newline_at = chunk.iter().position(|byte| *byte == b'\n');
            let (take, consumed, newline) = match newline_at {
                Some(end) => (end, end + 1, true),
                None => (chunk.len(), chunk.len(), false),
            };
            let room = LINE_LIMIT.saturating_sub(buffer.len());
            buffer.extend_from_slice(&chunk[..take.min(room)]);
            reader.consume(consumed);
            if newline {
                break;
            }
        }
        let line = String::from_utf8_lossy(&buffer).trim_end().to_owned();
        if lines.send(line).await.is_err() {
            return;
        }
    }
}

async fn finish(buffer: &[u8], lines: &mpsc::Sender<String>) {
    if !buffer.is_empty() {
        let _ = lines
            .send(String::from_utf8_lossy(buffer).trim_end().to_owned())
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::quick_host;

    #[test]
    fn the_quick_tunnel_address_is_read_from_its_log_line() {
        let line = "2026-11-02T10:00:00Z INF |  https://brave-owl-tree.trycloudflare.com  |";
        assert_eq!(
            quick_host(line).as_deref(),
            Some("brave-owl-tree.trycloudflare.com")
        );
        assert_eq!(
            quick_host(
                "failed to request quick Tunnel: Post \"https://api.trycloudflare.com/tunnel\""
            ),
            None
        );
        assert_eq!(quick_host("Visit https://example.com"), None);
        assert_eq!(quick_host("no address here"), None);
    }
}
