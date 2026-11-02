//! `[remote]`: how `bubblepaw remote serve` makes the host reachable.
use super::*;

/// How devices reach the host.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RemoteMode {
    /// Through the relay at `remote.relay`.
    #[default]
    Relay,
    /// A trycloudflare.com quick tunnel: no account, a new address on every
    /// start. Never started automatically.
    Quick,
    /// A named Cloudflare Tunnel at `remote.hostname` on the user's domain.
    Domain,
}

impl RemoteMode {
    /// Whether a service or host start may share the host in this mode without
    /// the user starting it. A quick tunnel's address changes every start and
    /// devices must pair again, so it only runs when started by hand.
    pub fn autostarts(self) -> bool {
        !matches!(self, Self::Quick)
    }
}

config_section! {
    /// Remote access (`bubblepaw remote ...`). Read at startup.
    #[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
    #[serde(default)]
    pub struct RemoteSettings {
        /// `bubblepaw remote serve` refuses to start unless this is true.
        pub enabled: bool => RESTART,
        /// `relay`, `quick` or `domain`.
        pub mode: RemoteMode => RESTART,
        /// The relay's WebSocket URL (`wss://...`; `ws://` for local testing).
        pub relay: Option<String> => RESTART,
        /// The host endpoint's port on 127.0.0.1 in `quick` and `domain` modes;
        /// 0 picks a free port.
        pub port: u16 => RESTART,
        /// The public hostname in `domain` mode, such as `paw.example.com`.
        pub hostname: Option<String> => RESTART,
        /// Path of the `cloudflared` program; by default the copy from `remote
        /// tunnel install`, else `cloudflared` on PATH.
        pub cloudflared: Option<String> => RESTART,
    }
}
impl RemoteSettings {
    pub(super) fn validate(&self) -> Result<()> {
        if let Some(relay) = &self.relay {
            ensure!(
                relay.starts_with("wss://") || relay.starts_with("ws://"),
                "remote.relay must be a ws:// or wss:// URL"
            );
        }
        if let Some(hostname) = &self.hostname {
            crate::remote::check_hostname(hostname)?;
        }
        if let Some(path) = &self.cloudflared {
            ensure!(
                !path.trim().is_empty(),
                "remote.cloudflared must not be empty"
            );
        }
        Ok(())
    }
}
