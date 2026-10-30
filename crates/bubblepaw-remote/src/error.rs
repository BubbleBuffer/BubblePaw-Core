/// Failures of the remote protocol. Pairing failures remain distinguishable.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("pairing store was replaced, but durability could not be confirmed: {0}")]
    StoreDurability(#[source] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Noise(#[from] snow::Error),
    #[error(transparent)]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error(transparent)]
    Key(#[from] crate::device::DeviceKeyError),
    #[error("protocol error: {0}")]
    Protocol(&'static str),
    #[error("frame exceeds the configured maximum")]
    OversizedFrame,
    #[error("peer's static key is not paired")]
    UnpairedPeer,
    #[error("pairing code is incorrect")]
    WrongCode,
    #[error("pairing code expired")]
    ExpiredCode,
    #[error("pairing code has already been used")]
    ReusedCode,
    #[error("pairing comparison was declined")]
    ComparisonDeclined,
    #[error("pairing comparison does not match")]
    ComparisonMismatch,
    #[error("relay authentication failed")]
    AuthenticationFailed,
    #[error("relay limit reached: {0}")]
    Limit(&'static str),
    #[error("connection idle timeout")]
    IdleTimeout,
    #[error("connection closed")]
    Closed,
}

pub type Result<T> = std::result::Result<T, Error>;
