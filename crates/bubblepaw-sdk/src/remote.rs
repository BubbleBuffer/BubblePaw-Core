//! Remote access, end to end encrypted, through a relay or the host's own
//! WebSocket endpoint (published by a Cloudflare Tunnel, see docs/remote.md).
//!
//! On the host, [`crate::Client::serve_remote`] (or [`RemoteServer::start`])
//! makes the host reachable as [`RemoteReach`] says and serves paired devices
//! with the profile each was paired with; [`RemoteServer::pair`] opens a
//! pairing offer and [`RemoteServer::address`] says where devices reach the
//! host. On a device, [`pair`] proves the offer's code (take the address from
//! the offer's [`PairingPayload`]) and [`PendingPairing::finish`] stores the
//! host once the host approves; [`crate::Client::connect_remote`] then
//! returns a client with the same calls and notifications as a local one.
pub use bubblepaw_runner::config::{RemoteMode, RemoteSettings};
pub use bubblepaw_runner::remote::{
    CLOUDFLARE_TOKEN_ENV, CLOUDFLARED_VERSION, CloudflareAccount, CloudflareApi, CloudflareLogin,
    CloudflareTunnel, CloudflareZone, CloudflaredInfo, DeviceKeyChange, DeviceKeyInfo,
    DeviceKeyPaths, DeviceKind, HostPresence, InstallSource, Limits, PairedDevice, PairedHostInfo,
    PairingOffer, PairingPayload, PairingRequest, PendingPairing, ProcessKind, RemoteAddress,
    RemoteConnect, RemoteError, RemotePair, RemotePaths, RemoteReach, RemoteServeOptions,
    RemoteServer, RemoteStatus, SavedHost, ServeStatus, SessionBackend, TokenSource, TunnelLogin,
    TunnelMode, check_hostname, cloudflare_account, cloudflare_login_state, cloudflared_info,
    device_key_info, export_device_key, find_cloudflared, forget_cloudflare_token, forget_host,
    import_device_key, install::install as install_cloudflared, list_devices, list_hosts,
    load_or_create_key, pair, probe_host, regenerate_device_key, revoke_device, serve_status,
    store_cloudflare_token, tunnel_login, update_cloudflared,
};
