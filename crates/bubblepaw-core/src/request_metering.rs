//! Per-request billing records of a run, kept in core state between the model call
//! that makes them and the journal commit that writes them. The run loop does not
//! see them: a provider wrapper opens the capture scope around the call, the request
//! context settles the result into the ledger, and the journal drains the ledger in
//! the commits that used to receive the records from the loop.
use crate::{
    CancellationToken, ModelRequest, ModelResponse, Provider, RequestUsage, Store, StreamCallback,
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use std::{collections::HashMap, sync::Arc, sync::Mutex};

#[derive(Default)]
struct Held {
    /// Records of ended requests that no commit has written yet, oldest first.
    settled: Vec<RequestUsage>,
    /// What the current request's provider call captured, before it ended.
    attempts: Vec<RequestUsage>,
    /// Ids of the captured attempts whose output the reply message owns.
    links: Vec<String>,
    /// The record the current request falls back to when nothing was captured.
    fallback: Option<RequestUsage>,
}

/// Records and message links by run, until a commit writes or drops them.
#[derive(Default)]
pub(crate) struct UsageLedger {
    runs: Mutex<HashMap<String, Held>>,
}

impl UsageLedger {
    fn with<T>(&self, run_id: &str, f: impl FnOnce(&mut Held) -> T) -> T {
        f(self
            .runs
            .lock()
            .unwrap()
            .entry(run_id.to_owned())
            .or_default())
    }
    /// A request is about to start; this is the record it falls back to.
    pub(crate) fn expect(&self, run_id: &str, fallback: RequestUsage) {
        self.with(run_id, |held| held.fallback = Some(fallback));
    }
    /// The provider call returned what its capture scope collected.
    fn reported(&self, run_id: &str, attempts: Vec<RequestUsage>, links: Vec<String>) {
        self.with(run_id, |held| {
            held.attempts = attempts;
            held.links = links;
        });
    }
    /// The request ended with this result. Without captured attempts the fallback
    /// stands in for them. The records now wait for a commit; the one the request
    /// is read by is returned.
    pub(crate) fn ended(
        &self,
        run_id: &str,
        result: &Result<ModelResponse>,
    ) -> Option<RequestUsage> {
        self.with(run_id, |held| {
            let mut attempts = std::mem::take(&mut held.attempts);
            let fallback = held.fallback.take();
            if attempts.is_empty()
                && let Some(fallback) = fallback
            {
                attempts.push(crate::request_usage::fallback_usage(
                    fallback,
                    result,
                    &mut held.links,
                ));
            }
            let read = attempts
                .iter()
                .rev()
                .find(|record| !record.failed)
                .or(attempts.last())
                .cloned();
            held.settled.extend(attempts);
            read
        })
    }
    /// What a commit writes now, without letting go of it.
    pub(crate) fn waiting(&self, run_id: &str) -> (Vec<RequestUsage>, Vec<String>) {
        self.with(run_id, |held| (held.settled.clone(), held.links.clone()))
    }
    /// A commit wrote what `waiting` returned.
    pub(crate) fn written(&self, run_id: &str) {
        self.release(run_id);
    }
    /// What the run's last commit writes; nothing stays behind, written or not.
    pub(crate) fn take(&self, run_id: &str) -> (Vec<RequestUsage>, Vec<String>) {
        self.runs
            .lock()
            .unwrap()
            .remove(run_id)
            .map_or_else(Default::default, |held| (held.settled, held.links))
    }
    /// The run's records are let go unwritten.
    pub(crate) fn release(&self, run_id: &str) {
        self.runs.lock().unwrap().remove(run_id);
    }
}

/// Lets the run's records go when the run's future ends or is dropped, so a run
/// that panics or is aborted leaves nothing for a later commit to pick up.
pub(crate) struct RunHold {
    pub(crate) state: Arc<crate::runtime::RuntimeState>,
    pub(crate) run_id: String,
}
impl Drop for RunHold {
    fn drop(&mut self) {
        self.state.usage.release(&self.run_id);
    }
}

/// The configured provider with the main request's capture scope around `generate`.
/// Cancellation is raced inside the scope, so a cancelled call still returns its
/// records; a call dropped mid-flight writes them to the store when the scope drops.
pub(crate) struct Metered {
    pub(crate) inner: Arc<dyn Provider>,
    pub(crate) store: Store,
    pub(crate) state: Arc<crate::runtime::RuntimeState>,
    pub(crate) session_id: String,
    pub(crate) run_id: String,
}

#[async_trait]
impl Provider for Metered {
    async fn generate(
        &self,
        request: ModelRequest,
        on_text: StreamCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let (result, attempts, links) = crate::request_usage::capture_message_request_usage(
            &self.store,
            &self.session_id,
            &self.run_id,
            async {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => Err(anyhow!("Cancelled during provider request")),
                    result = self.inner.generate(request, on_text, cancellation.clone()) => result,
                }
            },
        )
        .await;
        self.state.usage.reported(&self.run_id, attempts, links);
        result
    }
}
