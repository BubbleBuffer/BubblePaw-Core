//! The guard that cancels an abandoned step.
use super::Connection;
use bubblepaw_process::ProcessTree;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::{process::Child, sync::mpsc, task::JoinHandle};

/// How long a dropped connection's writer may take to send what is queued.
const FLUSH: Duration = Duration::from_secs(1);

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

/// Tear a dropped connection down. The runtime drops a step's provider future
/// the moment a run is cancelled, so `StepGuard` queues its `cancel` just
/// before the connection is dropped. Killing the extension and aborting the writer
/// task at once would lose that line; the queue is closed here, so the writer
/// drains it and ends, and only then is the tree killed. `tasks[0]` is the writer.
pub(super) fn retire(connection: &mut Connection) {
    // Closing the queue now (not after this call) lets the writer end once drained.
    drop(std::mem::replace(
        &mut connection.outgoing,
        mpsc::unbounded_channel().0,
    ));
    let child = connection._child.take();
    let tree = connection.tree.clone();
    let tasks = std::mem::take(&mut connection.tasks);
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return kill(child, &tree, tasks);
    };
    runtime.spawn(async move {
        let mut tasks = tasks;
        if let Some(writer) = tasks.first_mut() {
            let _ = tokio::time::timeout(FLUSH, writer).await;
        }
        kill(child, &tree, tasks);
    });
}

fn kill(child: Option<Child>, tree: &ProcessTree, tasks: Vec<JoinHandle<()>>) {
    tree.kill();
    for task in tasks {
        task.abort();
    }
    drop(child);
}
