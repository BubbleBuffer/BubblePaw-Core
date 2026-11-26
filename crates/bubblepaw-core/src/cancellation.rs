//! The token that cancels a run, a tool call or a provider request.
use std::sync::Arc;
use tokio::sync::watch;

#[derive(Clone, Debug)]
pub struct CancellationToken(Arc<watch::Sender<bool>>);
impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}
impl CancellationToken {
    pub fn new() -> Self {
        let (sender, _) = watch::channel(false);
        Self(Arc::new(sender))
    }
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
    pub async fn cancelled(&self) {
        let mut receiver = self.0.subscribe();
        while !*receiver.borrow_and_update() {
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}
