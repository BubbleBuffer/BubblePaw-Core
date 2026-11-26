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
/// A single-tool run as the shell holds it: the loop's record of it, and where
/// its completion goes.
pub(super) struct SingleToolHandle {
    pub(super) run: SingleToolRun,
    pub(super) completion: tokio::sync::oneshot::Sender<Run>,
}
