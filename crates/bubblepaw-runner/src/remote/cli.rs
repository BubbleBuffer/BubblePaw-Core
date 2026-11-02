//! `bubblepaw remote serve | pair | devices`.
use super::{
    PairingOffer, RemotePaths, RemoteServeOptions, RemoteServer, SessionBackend, list_devices,
    revoke_device,
};
use crate::cli::{Cli, Command};
use crate::config::Settings;
use crate::host_api::{HostCtx, Profile};
use anyhow::{Context, Result, bail};
use bubblepaw_core::CancellationToken;
use clap::{Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

/// How long `remote pair` waits for the relay to accept the host.
const ONLINE_WAIT: Duration = Duration::from_secs(30);
/// How often attached serving checks that the local host is still there.
const HOST_PROBE: Duration = Duration::from_secs(5);

#[derive(Subcommand, Clone, Serialize, Deserialize)]
pub enum RemoteCommand {
    /// Serve this host to paired devices through the configured relay (foreground).
    Serve,
    /// Pair a device: shows a QR code and a one-time code valid for 5 minutes.
    Pair {
        /// What the device may do.
        #[arg(long, value_enum, default_value_t = ProfileArg::Read)]
        profile: ProfileArg,
    },
    /// List paired devices.
    Devices {
        #[command(subcommand)]
        command: Option<DevicesCommand>,
    },
}

#[derive(Subcommand, Clone, Serialize, Deserialize)]
pub enum DevicesCommand {
    /// Remove a paired device; a running `remote serve` closes its tunnels.
    Revoke { id: String },
}

#[derive(Clone, Copy, Debug, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileArg {
    Read,
    Edit,
    Full,
}

impl From<ProfileArg> for Profile {
    fn from(value: ProfileArg) -> Self {
        match value {
            ProfileArg::Read => Profile::Read,
            ProfileArg::Edit => Profile::Edit,
            ProfileArg::Full => Profile::Full,
        }
    }
}

fn relay(settings: &Settings) -> Result<String> {
    if !settings.remote.enabled {
        bail!("remote access is off; set remote.enabled = true and remote.relay in the config");
    }
    settings
        .remote
        .relay
        .clone()
        .context("remote.relay is not set; set it to the relay's wss:// URL")
}

fn host_name() -> String {
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(|name| name.trim().to_owned())
        .find(|name| !name.is_empty() && name.len() <= 64)
        .unwrap_or_else(|| "BubblePaw host".to_owned())
}

/// Held while this process uses the host's relay registration.
fn lock(paths: &RemotePaths) -> Result<std::fs::File> {
    use fs2::FileExt;
    if let Some(parent) = paths.lock.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&paths.lock)
        .with_context(|| format!("could not open {}", paths.lock.display()))?;
    if file.try_lock_exclusive().is_err() {
        bail!(
            "another `bubblepaw remote serve` or `remote pair` is running for this data directory; \
             stop it first (pairing while serving is not supported from a second process)"
        );
    }
    Ok(file)
}

/// Commands that run before the store opens: `pair`, `devices`, and `serve`
/// when another process owns the store (served through that host). `None`
/// lets `serve` continue to own the store itself.
pub async fn early(cli: &Cli, settings: &Settings, data_dir: &Path) -> Option<Result<()>> {
    let Command::Remote { command } = &cli.command else {
        return None;
    };
    let paths = RemotePaths::in_data_dir(data_dir);
    match command {
        RemoteCommand::Devices { command: None } => Some(devices(&paths, cli.json)),
        RemoteCommand::Devices {
            command: Some(DevicesCommand::Revoke { id }),
        } => Some(revoke(&paths, id)),
        RemoteCommand::Pair { profile } => Some(pair(settings, data_dir, (*profile).into()).await),
        RemoteCommand::Serve => {
            if let Err(error) = relay(settings) {
                return Some(Err(error));
            }
            let probe = crate::host_transport::connect(data_dir, Duration::from_millis(200)).await;
            match probe {
                Ok(connection) => {
                    drop(connection);
                    Some(serve_attached(settings, data_dir).await)
                }
                Err(_) => None,
            }
        }
    }
}

fn devices(paths: &RemotePaths, json: bool) -> Result<()> {
    let devices = list_devices(&paths.clients)?;
    let mut out = std::io::stdout().lock();
    if json {
        serde_json::to_writer(&mut out, &devices)?;
        writeln!(out)?;
        return Ok(());
    }
    if devices.is_empty() {
        writeln!(out, "No paired devices.")?;
    }
    for device in devices {
        let paired = device
            .paired_at
            .and_then(|seconds| chrono::DateTime::from_timestamp(seconds as i64, 0))
            .map_or_else(
                || "-".to_owned(),
                |date| date.format("%Y-%m-%d").to_string(),
            );
        let profile = serde_json::to_value(device.profile)?;
        let profile = profile.as_str().unwrap_or_default();
        writeln!(
            out,
            "{}  {:<5} {}  {}",
            device.id, profile, paired, device.label
        )?;
    }
    Ok(())
}

fn revoke(paths: &RemotePaths, id: &str) -> Result<()> {
    if !revoke_device(&paths.clients, id)? {
        bail!("no paired device {id}");
    }
    println!("Revoked {id}. A running `remote serve` closes its tunnels within a second.");
    Ok(())
}

fn print_offer(offer: &PairingOffer) -> Result<()> {
    let payload = offer.qr_payload().encode();
    let qr = qrcode::QrCode::new(payload.as_bytes()).context("could not build the QR code")?;
    let image = qr
        .render::<qrcode::render::unicode::Dense1x2>()
        .dark_color(qrcode::render::unicode::Dense1x2::Light)
        .light_color(qrcode::render::unicode::Dense1x2::Dark)
        .quiet_zone(true)
        .build();
    let mut out = std::io::stdout().lock();
    writeln!(out, "{image}")?;
    writeln!(out, "Pairing code: {}", offer.code())?;
    writeln!(
        out,
        "Scan the QR code or enter the code on the device. It expires in {} minutes.",
        offer.expires_in().as_secs().div_ceil(60)
    )?;
    out.flush()?;
    Ok(())
}

async fn confirm(question: String) -> bool {
    tokio::task::spawn_blocking(move || {
        print!("{question} [y/N] ");
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).is_ok()
            && matches!(answer.trim(), "y" | "Y" | "yes" | "Yes")
    })
    .await
    .unwrap_or(false)
}

async fn pair(settings: &Settings, data_dir: &Path, profile: Profile) -> Result<()> {
    let relay = relay(settings)?;
    let paths = RemotePaths::in_data_dir(data_dir);
    let _lock = lock(&paths)?;
    let options = RemoteServeOptions::in_data_dir(data_dir, relay, host_name());
    let server = RemoteServer::start(options, None)?;
    server.wait_online(ONLINE_WAIT).await?;
    let mut offer = server.pair(profile).await?;
    print_offer(&offer)?;
    let request = tokio::select! {
        request = offer.next() => request,
        _ = tokio::signal::ctrl_c() => None,
    };
    let Some(request) = request else {
        server.shutdown().await;
        bail!("no device paired before the code expired");
    };
    let question = format!(
        "Device \"{}\" ({}) wants to pair.\nCheck number: {}. Does the device show the same number?",
        request.device_name(),
        request.device_id(),
        request.comparison()
    );
    let result = if confirm(question).await {
        let device = request.approve(None).await?;
        println!(
            "Paired {} ({}) with the {} profile.",
            device.label,
            device.id,
            serde_json::to_value(device.profile)?
                .as_str()
                .unwrap_or_default()
        );
        Ok(())
    } else {
        request.decline();
        Err(anyhow::anyhow!("pairing declined; nothing was stored"))
    };
    // Let the decision reach the device before the registration closes.
    tokio::time::sleep(Duration::from_millis(500)).await;
    server.shutdown().await;
    result
}

async fn run_server(
    server: RemoteServer,
    stop: impl std::future::Future<Output = Result<()>>,
) -> Result<()> {
    eprintln!(
        "Remote host {} serving through the relay; press Ctrl-C to stop.",
        server.host_id()
    );
    let result = tokio::select! {
        result = stop => result,
        _ = tokio::signal::ctrl_c() => Ok(()),
    };
    server.shutdown().await;
    result
}

/// Serve through the host that owns the store. Ends when that host stops.
async fn serve_attached(settings: &Settings, data_dir: &Path) -> Result<()> {
    let relay = relay(settings)?;
    let paths = RemotePaths::in_data_dir(data_dir);
    let _lock = lock(&paths)?;
    let options = RemoteServeOptions::in_data_dir(data_dir, relay, host_name());
    let backend = SessionBackend::Attach(data_dir.to_owned());
    let server = RemoteServer::start(options, Some(backend))?;
    eprintln!("Serving through the running host for this data directory.");
    let host_gone = async {
        loop {
            tokio::time::sleep(HOST_PROBE).await;
            let probe = crate::host_transport::connect(data_dir, Duration::from_secs(1)).await;
            if probe.is_err() {
                bail!("the local host stopped; run `bubblepaw remote serve` again");
            }
        }
    };
    run_server(server, host_gone).await
}

/// `remote serve` in the process that owns the store.
pub async fn serve(ctx: &HostCtx, settings: &Settings, cancel: &CancellationToken) -> Result<()> {
    let relay = relay(settings)?;
    let data_dir = ctx
        .store
        .data_dir()
        .context("host has no data directory")?
        .to_owned();
    let paths = RemotePaths::in_data_dir(&data_dir);
    let _lock = lock(&paths)?;
    let options = RemoteServeOptions::in_data_dir(&data_dir, relay, host_name());
    let server = RemoteServer::start(
        options,
        Some(SessionBackend::Context(Box::new(ctx.clone()))),
    )?;
    run_server(server, async {
        cancel.cancelled().await;
        Ok(())
    })
    .await
}
