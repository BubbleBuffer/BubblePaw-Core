//! Records owned by one runtime and its completion boundary.
use super::*;

pub(crate) struct Active {
    pub(super) run_id: String,
    pub(super) cancellation: CancellationToken,
    pub(super) notify: Arc<Notify>,
    pub(super) finished: Option<Run>,
    /// The turn panicked; `finished` is the failed record every waiter reports.
    pub(super) panicked: Option<String>,
    /// The settled boundary is committed; maintenance precedes idle notification.
    pub(super) finalizing: bool,
    /// The client whose cancel stopped this run; RunFinished carries it.
    pub(super) cancelled_by: Option<ClientInfo>,
}
pub(super) struct CallbackLifetime(pub(super) Arc<AtomicBool>);
pub(super) struct TurnOutcome {
    pub(super) text: String,
    pub(super) result_message_id: String,
    pub(super) final_message: Option<Message>,
    pub(super) request_ids: Vec<String>,
}
pub(super) struct SingleToolRun {
    pub(super) call: ToolCall,
    pub(super) result: Arc<Mutex<Option<ToolResult>>>,
    pub(super) completion: tokio::sync::oneshot::Sender<Run>,
}
impl Drop for CallbackLifetime {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}
