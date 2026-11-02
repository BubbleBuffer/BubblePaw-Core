//! Remote access through a relay, end to end encrypted.
//!
//! On the host, [`crate::Client::serve_remote`] (or [`RemoteServer::start`])
//! registers at the relay and serves paired devices with the profile each was
//! paired with; [`RemoteServer::pair`] opens a pairing offer. On a device,
//! [`pair`] proves the offer's code and [`PendingPairing::finish`] stores the
//! host once the host approves; [`crate::Client::connect_remote`] then
//! returns a client with the same calls and notifications as a local one.
pub use bubblepaw_runner::remote::{
    Limits, PairedDevice, PairedHostInfo, PairingOffer, PairingPayload, PairingRequest,
    PendingPairing, RemoteConnect, RemoteError, RemotePair, RemotePaths, RemoteServeOptions,
    RemoteServer, RemoteStatus, SessionBackend, list_devices, load_or_create_key, pair,
    revoke_device,
};
