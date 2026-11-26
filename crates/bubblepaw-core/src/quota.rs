//! Provider observations, independent of billing estimates and host routing policy.
use crate::{EventKind, QuotaWait, Store};
use anyhow::Result;
use std::future::Future;
use std::sync::Arc;

/// A scheduled resume lost ownership to a cancel, explicit run or newer wait.
#[derive(Clone, Debug)]
pub struct QuotaWaitChanged {
    pub session_id: String,
    pub run_id: String,
}
impl std::fmt::Display for QuotaWaitChanged {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Quota wait changed before automatic resume for session {}",
            self.session_id
        )
    }
}
impl std::error::Error for QuotaWaitChanged {}

/// Who owns a wait at this moment: the run whose turn is being polled.
type Current = Arc<std::sync::Mutex<Option<(String, String)>>>;

struct WaitContext {
    store: Store,
    current: Current,
}
tokio::task_local! { static WAIT_CONTEXT: WaitContext; }

/// Enter the wait context for a whole drive task. A wait finds an owner only while
/// a `TurnScope` is alive, so each run of the task is its own owner.
pub(crate) async fn with_wait_context<F: Future>(store: &Store, future: F) -> F::Output {
    WAIT_CONTEXT
        .scope(
            WaitContext {
                store: store.clone(),
                current: Current::default(),
            },
            future,
        )
        .await
}

/// Names the run as the owner of waits for as long as it lives; dropping it, or
/// the future that holds it, leaves no owner.
pub(crate) struct TurnScope(Option<Current>);

impl TurnScope {
    pub(crate) fn enter(session_id: &str, run_id: &str) -> Self {
        let current = WAIT_CONTEXT.try_with(|c| c.current.clone()).ok();
        if let Some(current) = &current {
            *current.lock().unwrap() = Some((session_id.into(), run_id.into()));
        }
        Self(current)
    }
}

impl Drop for TurnScope {
    fn drop(&mut self) {
        if let Some(current) = &self.0 {
            *current.lock().unwrap() = None;
        }
    }
}

/// Persist a wait before sleeping. False means there is no owning harness turn;
/// errors (especially unresolved effects) must abort parking, not be ignored.
pub fn set_quota_wait(account: &str, window: &str, resume_at: &str, reason: &str) -> Result<bool> {
    WAIT_CONTEXT
        .try_with(|c| {
            let Some((session_id, run_id)) = c.current.lock().unwrap().clone() else {
                return Ok(false);
            };
            let wait = QuotaWait {
                session_id,
                run_id,
                account: account.into(),
                window: window.into(),
                resume_at: resume_at.into(),
                reason: reason.into(),
            };
            c.store.put_quota_wait(&wait)?;
            Ok(true)
        })
        .unwrap_or(Ok(false))
}

/// Clear immediately before retry dispatch. A crash after clearing is an
/// ordinary interruption and cannot trigger automatic quota recovery.
pub fn clear_current_quota_wait() -> Result<bool> {
    WAIT_CONTEXT
        .try_with(|c| {
            let owner = c.current.lock().unwrap().clone();
            match owner {
                Some((session_id, run_id)) => c.store.clear_quota_wait(&session_id, &run_id),
                None => Ok(false),
            }
        })
        .unwrap_or(Ok(false))
}

pub(crate) fn waiting_event(wait: &QuotaWait) -> EventKind {
    EventKind::RunWaitingForQuota { wait: wait.clone() }
}
