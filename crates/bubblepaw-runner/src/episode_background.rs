//! One host-owned listener schedules shared coverage for root, native child and
//! hidden parallel-worker sessions. Gathering never changes their serving views.
use crate::{episode_worker::EpisodeWorker, summarization::SummarizingContext};
use anyhow::Result;
use bubblepaw_core::{ContextPreparation, EventKind, HarnessEvent, ModelRequest, Store};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{broadcast, oneshot},
    task::JoinHandle,
};

/// The request an episode trigger starts from, resolved for each session it wakes.
pub type EpisodeTemplate = Arc<dyn Fn(&str) -> Result<ModelRequest> + Send + Sync>;

pub struct EpisodeBackground {
    stop: Option<oneshot::Sender<()>>,
    listener: Option<JoinHandle<()>>,
    worker: Arc<EpisodeWorker>,
    store: Store,
    cleanup_name: String,
}

static NEXT_OWNER: AtomicU64 = AtomicU64::new(0);

impl EpisodeBackground {
    /// A one-shot run waits this long for in-flight gathering so its usage is
    /// committed before exit.
    pub const ONE_SHOT_DRAIN: Duration = Duration::from_secs(60);
    /// A long-lived host (stdio, web) keeps a short bounded shutdown.
    pub const HOST_DRAIN: Duration = Duration::from_secs(5);

    /// The root harness forwards native-child and hidden-worker events here. All
    /// profiles share this worker with synchronous catch-up; children must not
    /// start separate listeners or own independent coverage tasks. This listener
    /// owns the worker's host lifetime: shutdown/drop permanently stops it, so a
    /// new host creates a new worker rather than reusing the stopped instance.
    pub fn start(
        events: broadcast::Receiver<HarnessEvent>,
        store: Store,
        summary: Arc<SummarizingContext>,
        worker: Arc<EpisodeWorker>,
        template: EpisodeTemplate,
    ) -> Self {
        Self::start_scoped(
            events,
            store,
            summary,
            worker,
            template,
            crate::session_scope::SessionScope::all(),
        )
    }

    pub fn start_scoped(
        mut events: broadcast::Receiver<HarnessEvent>,
        store: Store,
        summary: Arc<SummarizingContext>,
        worker: Arc<EpisodeWorker>,
        template: EpisodeTemplate,
        scope: crate::session_scope::SessionScope,
    ) -> Self {
        let (stop, mut stopped) = oneshot::channel();
        let weak_worker = Arc::downgrade(&worker);
        let cleanup_name = format!(
            "runner.episodes.{}",
            NEXT_OWNER.fetch_add(1, Ordering::Relaxed)
        );
        store.set_session_cleanup(
            cleanup_name.clone(),
            Arc::new(move |session| {
                if let Some(worker) = weak_worker.upgrade() {
                    worker.cancel_session(session);
                }
                Ok(())
            }),
        );
        let owner_store = store.clone();
        let target = worker.clone();
        let listener = tokio::spawn(async move {
            let wake_session = |session: &str, run: Option<&str>| {
                if !scope.contains(&store, session) {
                    return;
                }
                // Consult the catalog before any session-scoped history access.
                // Retention can remove transient chats while stale events remain.
                if !store
                    .active_session_metadata(session)
                    .is_ok_and(|live| live.is_some())
                {
                    target.cancel_session(session);
                    return;
                }
                // LCM eligibility comes from canonical source coverage. It does
                // not need to decode the serving history or measure its pressure.
                if summary.trigger_lcm_worker(&target, session).unwrap_or(true) {
                    return;
                }
                let history = match summary.history_frontier(session) {
                    Ok(Some(frontier)) => store
                        .messages_after(session, frontier)
                        .map(|rows| rows.into_iter().map(|(_, message)| message).collect()),
                    Ok(None) => store.serving_messages(session),
                    Err(error) => Err(error),
                };
                if let Ok(messages) = history {
                    // A template that cannot be built skips this trigger, like other failures.
                    let Ok(mut request) = template(session) else {
                        return;
                    };
                    request.messages = messages;
                    if let Some(run) = run.filter(|run| !run.is_empty())
                        && let Ok(run) = store.run(run)
                    {
                        request.model = run.model;
                    }
                    // Archive/model failures leave coverage pending, never failing a run.
                    let _ = summary.trigger_episode_worker(&target, session, request);
                }
            };
            let wake = |event: HarnessEvent| {
                if matches!(
                    event.kind,
                    EventKind::ToolStarted { .. }
                        | EventKind::ToolFinished { .. }
                        | EventKind::RunFinished { .. }
                        | EventKind::SessionSettled {}
                ) {
                    wake_session(&event.session_id, Some(&event.run_id));
                }
            };
            let rescan = || {
                if let Ok(sessions) = store.list_sessions() {
                    let sessions: Vec<_> = sessions
                        .into_iter()
                        .filter(|session| scope.contains(&store, &session.id))
                        .collect();
                    target.reconcile_sessions(
                        &sessions
                            .iter()
                            .map(|session| session.id.clone())
                            .collect::<Vec<_>>(),
                    );
                    for session in sessions {
                        wake_session(&session.id, None);
                    }
                }
            };
            loop {
                tokio::select! {
                    biased;
                    _ = &mut stopped => {
                        // A short CLI run may stop before the listener runs. Recover
                        // lag here too so hidden workers get the same final drain.
                        loop {
                            match events.try_recv() {
                                Ok(event) => wake(event),
                                Err(broadcast::error::TryRecvError::Lagged(_)) => rescan(),
                                Err(_) => break,
                            }
                        }
                        break;
                    }
                    received = events.recv() => match received {
                        Ok(event) => wake(event),
                        Err(broadcast::error::RecvError::Lagged(_)) => rescan(),
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                }
            }
        });
        Self {
            stop: Some(stop),
            listener: Some(listener),
            worker,
            store: owner_store,
            cleanup_name,
        }
    }

    pub async fn shutdown(mut self, timeout: Duration) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(listener) = self.listener.as_mut() {
            let _ = listener.await;
        }
        self.listener.take();
        if tokio::time::timeout(timeout, self.worker.shutdown_wait())
            .await
            .is_err()
        {
            self.worker.record_shutdown_timeout(timeout);
            self.worker.abort();
        }
    }
}

impl Drop for EpisodeBackground {
    fn drop(&mut self) {
        // A dropped host must not detach summary requests, including when an
        // enclosing shutdown future is itself cancelled.
        self.worker.abort();
        self.store.remove_session_cleanup(&self.cleanup_name);
        if let Some(listener) = &self.listener {
            listener.abort();
        }
    }
}
