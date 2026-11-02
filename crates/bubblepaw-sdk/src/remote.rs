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
pub use bubblepaw_runner::remote::{
    CLOUDFLARED_VERSION, CloudflareApi, CloudflareTunnel, InstallSource, Limits, PairedDevice,
    PairedHostInfo, PairingOffer, PairingPayload, PairingRequest, PendingPairing, RemoteAddress,
    RemoteConnect, RemoteError, RemotePair, RemotePaths, RemoteReach, RemoteServeOptions,
    RemoteServer, RemoteStatus, SessionBackend, TunnelMode, check_hostname, find_cloudflared,
    install::install as install_cloudflared, list_devices, load_or_create_key, pair, revoke_device,
};
