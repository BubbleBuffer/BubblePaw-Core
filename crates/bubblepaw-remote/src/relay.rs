//! Authenticated, ciphertext-only rendezvous over a plain WebSocket transport.
//! Terminate TLS at a reverse proxy, and expose only its `wss://` endpoint.
mod admission;
mod client;
mod router;
mod routing;
mod server;
mod wire;

pub use admission::{PermissiveAdmission, RelayAdmission, RelayProof, RelayRole};
pub use client::{Host, RelaySession, connect, connect_socket, register, register_socket};
pub use router::RelayRouter;
pub use server::{Relay, RelayConfig};

/// Resource limits shared by relay and connecting transports.
#[derive(Clone, Debug)]
pub struct Limits {
    pub max_frame_bytes: usize,
    pub idle_timeout: std::time::Duration,
    pub connections_per_ip: usize,
    pub pending_per_host: usize,
    pub sessions_per_host: usize,
    pub queue_capacity: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_frame_bytes: 65_535,
            idle_timeout: std::time::Duration::from_secs(120),
            connections_per_ip: 32,
            pending_per_host: 16,
            sessions_per_host: 128,
            queue_capacity: 16,
        }
    }
}

impl Limits {
    /// Use before a caller-owned WebSocket upgrade to bound frame buffering.
    pub fn websocket_config(&self) -> tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
        wire::config(self)
    }

    fn validate(&self) -> crate::Result<()> {
        if self.max_frame_bytes == 0
            || self.max_frame_bytes > 65_535
            || self.idle_timeout.is_zero()
            || self.connections_per_ip == 0
            || self.pending_per_host == 0
            || self.sessions_per_host == 0
            || self.queue_capacity == 0
        {
            return Err(crate::Error::Protocol("invalid relay limits"));
        }
        Ok(())
    }
}
