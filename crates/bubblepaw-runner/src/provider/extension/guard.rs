//! The guard that cancels an abandoned step.
use super::Connection;
use serde_json::json;
use std::sync::Arc;

/// Sends `cancel` if the step is abandoned (for example when the harness drops
/// the provider future on cancellation) before its reply arrived.
pub(super) struct StepGuard {
    pub(super) connection: Arc<Connection>,
    pub(super) id: u64,
    pub(super) armed: bool,
    pub(super) cancel_sent: bool,
}
impl StepGuard {
    pub(super) fn cancel(&mut self) {
        if !self.cancel_sent {
            self.cancel_sent = true;
            self.connection.notify("cancel", json!({"id":self.id}));
        }
    }
}
impl Drop for StepGuard {
    fn drop(&mut self) {
        if self.armed {
            self.connection.forget(self.id);
            self.cancel();
        }
    }
}
