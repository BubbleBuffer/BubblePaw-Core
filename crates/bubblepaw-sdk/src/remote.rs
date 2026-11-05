//! Remote access, end to end encrypted, through a relay or the host's own
//! WebSocket endpoint (published by a Cloudflare Tunnel, see docs/remote.md).
//!
//! On the host (feature `host`), [`crate::Client::serve_remote`] (or [`RemoteServer::start`])
//! makes the host reachable as [`RemoteReach`] says and serves paired devices
//! with the profile each was paired with; [`RemoteServer::pair`] opens a
//! pairing offer and [`RemoteServer::address`] says where devices reach the
//! host. On a device, [`pair`] proves the offer's code (take the address from
//! the offer's [`PairingPayload`]) and [`PendingPairing::finish`] stores the
//! host once the host approves; [`crate::Client::connect_remote`] then
//! returns a client with the same calls and notifications as a local one.
#[cfg(feature = "host")]
pub use bubblepaw_runner::config::{RemoteMode, RemoteSettings};
#[cfg(feature = "host")]
pub use bubblepaw_runner::remote::{
    CLOUDFLARE_TOKEN_ENV, CLOUDFLARED_VERSION, CloudflareAccount, CloudflareApi, CloudflareLogin,
    CloudflareTunnel, CloudflareZone, CloudflaredInfo, InstallSource, PairedDevice, PairingOffer,
    PairingRequest, RemoteReach, RemoteServeOptions, RemoteServer, RemoteStatus, SessionBackend,
    TokenSource, TunnelLogin, TunnelMode, check_hostname, cloudflare_account,
    cloudflare_login_state, cloudflared_info, find_cloudflared, forget_cloudflare_login,
    forget_cloudflare_token, install::install as install_cloudflared, list_devices, revoke_device,
    store_cloudflare_token, tunnel_login, update_cloudflared,
};
pub use bubblepaw_runner::remote::{
    DeviceKeyChange, DeviceKeyInfo, DeviceKeyPaths, DeviceKind, HostPresence, Limits,
    PairedHostInfo, PairingPayload, PendingPairing, ProcessKind, RemoteAddress, RemoteConnect,
    RemoteError, RemotePair, RemotePaths, SavedHost, ServeStatus, device_key_info,
    export_device_key, forget_host, import_device_key, list_hosts, load_or_create_key, pair,
    probe_host, regenerate_device_key, serve_status,
};

#[cfg(feature = "host")]
/// Sending a provider's model traffic through a paired device (config
/// `providers.<name>.via`; the host user grants it with
/// [`RemoteServer::pair_with_egress`] or `remote pair --egress`, and
/// [`PairedDevice::egress`] / [`SavedHost::egress`] show it). See
/// `docs/remote.md`, "Model traffic through a paired device".
pub mod egress {
    pub use bubblepaw_runner::remote::egress::{
        EgressError, EgressErrorKind, EgressHandle, EgressProxy, EgressRoute, check_host,
    };
}
