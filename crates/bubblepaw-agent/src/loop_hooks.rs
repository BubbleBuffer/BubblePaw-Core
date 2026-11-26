//! What the loop calls out to for the shell: the default model and request
//! provider, and the work to do before a turn starts.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_ai::{CancellationToken, Provider};
use std::sync::Arc;

/// Resolve the host's default model and request client as one snapshot.
pub type ProviderResolver = Arc<dyn Fn() -> (String, Arc<dyn Provider>) + Send + Sync>;

/// Optional host work before a turn captures its tools or makes a model request.
#[async_trait]
pub trait TurnStart: Send + Sync {
    async fn before_turn(&self, session_id: &str, cancellation: CancellationToken) -> Result<()>;
}
