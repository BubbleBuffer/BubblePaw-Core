//! The CLI's reach and tunnel side: the reach the config asks for, `remote
//! tunnel install | login | token`, and pairing or revoking through a running
//! `remote serve`'s control endpoint.
use super::control::{Channel, Event, Request};
use super::reach::{CloudflareApi, CloudflareTunnel, RemoteReach, TunnelMode};
use super::{RemotePaths, domain, install};
use crate::config::{RemoteMode, Settings};
use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::Path;

#[derive(Subcommand, Clone, Serialize, Deserialize)]
pub enum TunnelCommand {
    /// Download the pinned cloudflared release into the data directory and
    /// verify its SHA-256.
    Install,
    /// Log cloudflared in to your Cloudflare account (`cloudflared tunnel
    /// login`), for domain mode without an API token.
    Login,
    /// Store a Cloudflare API token (Cloudflare Tunnel edit and DNS edit) for
    /// domain mode, read from stdin.
    Token,
}

/// The reach `[remote]` asks for.
pub(super) fn reach(settings: &Settings, paths: &RemotePaths) -> Result<RemoteReach> {
    let remote = &settings.remote;
    if !remote.enabled {
        bail!("remote access is off; set remote.enabled = true in the config (see docs/remote.md)");
    }
    let tunnel = |mode| -> Result<RemoteReach> {
        let configured = remote.cloudflared.as_deref().map(Path::new);
        Ok(RemoteReach::Cloudflare(CloudflareTunnel {
            mode,
            port: remote.port,
            cloudflared: super::find_cloudflared(configured, paths)?,
            dir: paths.cloudflare.clone(),
            api: None,
        }))
    };
    match remote.mode {
        RemoteMode::Relay => {
            Ok(RemoteReach::Relay(remote.relay.clone().context(
                "remote.relay is not set; set it to the relay's wss:// URL",
            )?))
        }
        RemoteMode::Quick => tunnel(TunnelMode::Quick),
        RemoteMode::Domain => {
            let hostname = remote.hostname.clone().context(
                "remote.mode = \"domain\" needs remote.hostname, such as paw.example.com",
            )?;
            let mut reach = tunnel(TunnelMode::Domain { hostname })?;
            if let RemoteReach::Cloudflare(tunnel) = &mut reach {
                tunnel.api = domain::api_token(&paths.cloudflare)?.map(CloudflareApi::new);
            }
            Ok(reach)
        }
    }
}

pub(super) async fn tunnel(
    command: &TunnelCommand,
    settings: &Settings,
    paths: &RemotePaths,
) -> Result<()> {
    match command {
        TunnelCommand::Install => {
            let source = install::InstallSource::default();
            let path = install::install(&paths.cloudflare_bin(), &source).await?;
            println!(
                "Installed cloudflared {} at {} (SHA-256 verified).",
                source.version,
                path.display()
            );
            Ok(())
        }
        TunnelCommand::Login => {
            let configured = settings.remote.cloudflared.as_deref().map(Path::new);
            let program = super::find_cloudflared(configured, paths)?;
            let status = tokio::process::Command::new(&program)
                .args(["tunnel", "login"])
                .status()
                .await
                .with_context(|| format!("could not run {}", program.display()))?;
            if !status.success() {
                bail!("cloudflared tunnel login failed ({status})");
            }
            println!(
                "Logged in. Set remote.mode = \"domain\" and remote.hostname, then run `bubblepaw remote serve`."
            );
            Ok(())
        }
        TunnelCommand::Token => {
            let mut token = String::new();
            std::io::stdin()
                .take(4096)
                .read_to_string(&mut token)
                .context("could not read the token from stdin")?;
            let token = zeroize::Zeroizing::new(token.trim().to_owned());
            if token.is_empty() || token.chars().any(char::is_whitespace) {
                bail!("expected one API token on stdin");
            }
            let path = paths.cloudflare.join(domain::TOKEN_FILE);
            domain::write_private(&path, token.as_bytes())?;
            println!("Stored the Cloudflare API token in {}.", path.display());
            Ok(())
        }
    }
}

/// What the CLI shows while pairing; implemented by `cli.rs`.
pub(super) trait PairingUi {
    fn offer(&self, payload: &str, code: &str, expires_in: u64) -> Result<()>;
    async fn confirm(&self, device_name: &str, device_id: &str, comparison: &str) -> bool;
    fn paired(&self, id: &str, label: &str, profile: &str, egress: bool) -> Result<()>;
}

/// Pair through a running `remote serve`. `None` when none is running.
pub(super) async fn pair_through_server(
    paths: &RemotePaths,
    profile: super::cli::ProfileArg,
    egress: bool,
    ui: &impl PairingUi,
) -> Result<Option<()>> {
    let Some(mut channel) = Channel::connect(&paths.control).await? else {
        return Ok(None);
    };
    channel.write(&Request::Pair { profile, egress }).await?;
    loop {
        let event = tokio::select! {
            event = channel.read::<Event>() => event?,
            // Hanging up withdraws the offer.
            _ = tokio::signal::ctrl_c() => return Err(anyhow::anyhow!("pairing cancelled")),
        };
        match event.context("the running `remote serve` closed the connection")? {
            Event::Offer {
                payload,
                code,
                expires_in,
            } => ui.offer(&payload, &code, expires_in)?,
            Event::Request {
                device_name,
                device_id,
                comparison,
            } => {
                let answer = match ui.confirm(&device_name, &device_id, &comparison).await {
                    true => Request::Approve,
                    false => Request::Decline,
                };
                channel.write(&answer).await?;
            }
            Event::Paired {
                id,
                label,
                profile,
                egress,
            } => {
                ui.paired(&id, &label, &profile, egress)?;
                return Ok(Some(()));
            }
            Event::Declined => bail!("pairing declined; nothing was stored"),
            Event::Expired => bail!("no device paired before the code expired"),
            Event::Error { message } => bail!("{message}"),
            Event::Revoked { .. } => bail!("unexpected reply from `remote serve`"),
        }
    }
}

/// Revoke through a running `remote serve`, which closes the device's tunnels
/// at once. `None` when none is running.
pub(super) async fn revoke_through_server(paths: &RemotePaths, id: &str) -> Result<Option<bool>> {
    let Some(mut channel) = Channel::connect(&paths.control).await? else {
        return Ok(None);
    };
    channel
        .write(&Request::Revoke { id: id.to_owned() })
        .await?;
    match channel.read::<Event>().await? {
        Some(Event::Revoked { removed }) => Ok(Some(removed)),
        Some(Event::Error { message }) => bail!("{message}"),
        _ => bail!("unexpected reply from `remote serve`"),
    }
}
