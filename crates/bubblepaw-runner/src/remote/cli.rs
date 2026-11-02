//! `bubblepaw remote serve | pair | devices | tunnel`.
use super::cli_tunnel::{self, PairingUi, TunnelCommand};
use super::{
    ProcessKind, RemotePaths, RemoteServeOptions, RemoteServer, RemoteStatus, SessionBackend,
};
use super::{list_devices, revoke_device};
use crate::cli::{Cli, Command};
use crate::config::{RemoteMode, Settings};
use crate::host_api::{HostCtx, Profile};
use anyhow::{Context, Result, bail};
use bubblepaw_core::CancellationToken;
use clap::{Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

/// How long a standalone `remote pair` waits for the host to come online.
const ONLINE_WAIT: Duration = Duration::from_secs(90);
/// How often attached serving checks that the local host is still there.
const HOST_PROBE: Duration = Duration::from_secs(5);

#[derive(Subcommand, Clone, Serialize, Deserialize)]
pub enum RemoteCommand {
    /// Serve this host to paired devices as `[remote]` configures (foreground).
    Serve {
        /// Started by a service or login item: in quick mode, exit without
        /// sharing the host (quick tunnels only start by hand).
        #[arg(long)]
        autostart: bool,
    },
    /// Pair a device: shows a QR code and a one-time code valid for 5 minutes.
    /// Goes through a running `remote serve` when there is one.
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
    /// Set up Cloudflare Tunnel access.
    Tunnel {
        #[command(subcommand)]
        command: TunnelCommand,
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

fn host_name() -> String {
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(|name| name.trim().to_owned())
        .find(|name| !name.is_empty() && name.len() <= 64)
        .unwrap_or_else(|| "BubblePaw host".to_owned())
}

/// Commands that run before the store opens: `pair`, `devices`, `tunnel`,
/// and `serve` when another process owns the store (served through that
/// host). `None` lets `serve` continue to own the store itself.
pub async fn early(cli: &Cli, settings: &Settings, data_dir: &Path) -> Option<Result<()>> {
    let Command::Remote { command } = &cli.command else {
        return None;
    };
    let paths = RemotePaths::in_data_dir(data_dir);
    let ui = Ui { json: cli.json };
    match command {
        RemoteCommand::Devices { command: None } => Some(devices(&paths, cli.json)),
        RemoteCommand::Devices {
            command: Some(DevicesCommand::Revoke { id }),
        } => Some(revoke(&paths, id).await),
        RemoteCommand::Pair { profile } => Some(pair(settings, data_dir, *profile, &ui).await),
        RemoteCommand::Tunnel { command } => {
            Some(cli_tunnel::tunnel(command, settings, &paths).await)
        }
        RemoteCommand::Serve { autostart } => {
            if *autostart && !settings.remote.mode.autostarts() {
                eprintln!(
                    "remote.mode = \"quick\" does not start automatically; the host stays \
                     unshared. Run `bubblepaw remote serve` by hand to share it."
                );
                return Some(Ok(()));
            }
            if let Err(error) = cli_tunnel::reach(settings, &paths) {
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

async fn revoke(paths: &RemotePaths, id: &str) -> Result<()> {
    let removed = match cli_tunnel::revoke_through_server(paths, id).await? {
        Some(removed) => removed,
        None => revoke_device(&paths.clients, id)?,
    };
    if !removed {
        bail!("no paired device {id}");
    }
    println!("Revoked {id}. A running `remote serve` has closed its tunnels.");
    Ok(())
}

/// The terminal side of pairing; `--json` prints JSON lines instead.
struct Ui {
    json: bool,
}

impl PairingUi for Ui {
    fn offer(&self, payload: &str, code: &str, expires_in: u64) -> Result<()> {
        let mut out = std::io::stdout().lock();
        if self.json {
            let line = serde_json::json!({
                "event": "offer", "payload": payload, "code": code, "expires_in": expires_in,
            });
            writeln!(out, "{line}")?;
            out.flush()?;
            return Ok(());
        }
        let qr = qrcode::QrCode::new(payload.as_bytes()).context("could not build the QR code")?;
        let image = qr
            .render::<qrcode::render::unicode::Dense1x2>()
            .dark_color(qrcode::render::unicode::Dense1x2::Light)
            .light_color(qrcode::render::unicode::Dense1x2::Dark)
            .quiet_zone(true)
            .build();
        writeln!(out, "{image}")?;
        writeln!(out, "Pairing code: {code}")?;
        writeln!(
            out,
            "Scan the QR code or enter the code on the device. It expires in {} minutes.",
            expires_in.div_ceil(60)
        )?;
        out.flush()?;
        Ok(())
    }

    async fn confirm(&self, device_name: &str, device_id: &str, comparison: &str) -> bool {
        let question = match self.json {
            true => serde_json::json!({
                "event": "request", "device_name": device_name, "device_id": device_id,
                "comparison": comparison,
            })
            .to_string(),
            false => format!(
                "Device \"{device_name}\" ({device_id}) wants to pair.\nCheck number: \
                 {comparison}. Does the device show the same number? [y/N] "
            ),
        };
        let json = self.json;
        tokio::task::spawn_blocking(move || {
            match json {
                true => println!("{question}"),
                false => print!("{question}"),
            }
            let _ = std::io::stdout().flush();
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer).is_ok()
                && matches!(answer.trim(), "y" | "Y" | "yes" | "Yes")
        })
        .await
        .unwrap_or(false)
    }

    fn paired(&self, id: &str, label: &str, profile: &str) -> Result<()> {
        match self.json {
            true => println!(
                "{}",
                serde_json::json!({"event": "paired", "id": id, "label": label, "profile": profile})
            ),
            false => println!("Paired {label} ({id}) with the {profile} profile."),
        }
        Ok(())
    }
}

async fn pair(settings: &Settings, data_dir: &Path, profile: ProfileArg, ui: &Ui) -> Result<()> {
    let paths = RemotePaths::in_data_dir(data_dir);
    if cli_tunnel::pair_through_server(&paths, profile, ui)
        .await?
        .is_some()
    {
        return Ok(());
    }
    if settings.remote.enabled && settings.remote.mode == RemoteMode::Quick {
        bail!(
            "a quick tunnel's address only lives as long as `remote serve`; start `bubblepaw \
             remote serve` and run `bubblepaw remote pair` in another terminal"
        );
    }
    let reach = cli_tunnel::reach(settings, &paths)?;
    let mut options = RemoteServeOptions::in_data_dir(data_dir, reach, host_name());
    options.process = ProcessKind::Cli;
    let server = RemoteServer::start(options, None)?;
    server.wait_online(ONLINE_WAIT).await?;
    let mut offer = server.pair(profile.into()).await?;
    ui.offer(
        &offer.qr_payload().encode(),
        offer.code(),
        offer.expires_in().as_secs(),
    )?;
    let request = tokio::select! {
        request = offer.next() => request,
        _ = tokio::signal::ctrl_c() => None,
    };
    let Some(request) = request else {
        server.shutdown().await;
        bail!("no device paired before the code expired");
    };
    let approved = ui
        .confirm(
            request.device_name(),
            request.device_id(),
            request.comparison(),
        )
        .await;
    let result = if approved {
        let device = request.approve(None).await?;
        let profile = serde_json::to_value(device.profile)?;
        ui.paired(
            &device.id,
            &device.label,
            profile.as_str().unwrap_or_default(),
        )
    } else {
        request.decline();
        Err(anyhow::anyhow!("pairing declined; nothing was stored"))
    };
    // Let the decision reach the device before the host goes away.
    tokio::time::sleep(Duration::from_millis(500)).await;
    server.shutdown().await;
    result
}

async fn run_server(
    mut server: RemoteServer,
    paths: &RemotePaths,
    stop: impl std::future::Future<Output = Result<()>>,
) -> Result<()> {
    server.open_control(&paths.control).await?;
    eprintln!(
        "Remote host {} starting; press Ctrl-C to stop. Pair devices with `bubblepaw remote pair` \
         in another terminal.",
        server.host_id()
    );
    let mut address = server.watch_address();
    let mut status = server.watch_status();
    address.mark_changed();
    let mut announced = false;
    let mut stop = std::pin::pin!(stop);
    let result = loop {
        tokio::select! {
            result = &mut stop => break result,
            _ = tokio::signal::ctrl_c() => break Ok(()),
            changed = address.changed() => {
                if changed.is_err() {
                    break Ok(());
                }
                let current = address.borrow_and_update().clone();
                if let Some(current) = current {
                    if announced {
                        eprintln!("The address changed; devices paired at the old one must pair again.");
                    }
                    eprintln!("Devices reach this host at {}", current.url());
                    announced = true;
                }
            }
            changed = status.changed() => {
                if changed.is_err() {
                    break Ok(());
                }
                let current = *status.borrow_and_update();
                match current {
                    RemoteStatus::Online => eprintln!("Remote host online."),
                    RemoteStatus::Offline => eprintln!(
                        "Remote host offline: {}",
                        server.last_error().unwrap_or_default()
                    ),
                    RemoteStatus::Connecting => {}
                }
            }
        }
    };
    server.shutdown().await;
    result
}

/// Serve through the host that owns the store. Ends when that host stops.
async fn serve_attached(settings: &Settings, data_dir: &Path) -> Result<()> {
    let paths = RemotePaths::in_data_dir(data_dir);
    let reach = cli_tunnel::reach(settings, &paths)?;
    let mut options = RemoteServeOptions::in_data_dir(data_dir, reach, host_name());
    options.process = ProcessKind::Cli;
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
    run_server(server, &paths, host_gone).await
}

/// `remote serve` in the process that owns the store.
pub async fn serve(ctx: &HostCtx, settings: &Settings, cancel: &CancellationToken) -> Result<()> {
    let data_dir = ctx
        .store
        .data_dir()
        .context("host has no data directory")?
        .to_owned();
    let paths = RemotePaths::in_data_dir(&data_dir);
    let reach = cli_tunnel::reach(settings, &paths)?;
    let mut options = RemoteServeOptions::in_data_dir(&data_dir, reach, host_name());
    options.process = ProcessKind::Cli;
    let server = RemoteServer::start(
        options,
        Some(SessionBackend::Context(Box::new(ctx.clone()))),
    )?;
    run_server(server, &paths, async {
        cancel.cancelled().await;
        Ok(())
    })
    .await
}
