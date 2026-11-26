//! Provider observations, independent of billing estimates and host routing policy.
use crate::{EventKind, QuotaWait, Store};
use anyhow::Result;
use std::future::Future;

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

struct WaitContext {
    store: Store,
    session_id: String,
    run_id: String,
}
tokio::task_local! { static WAIT_CONTEXT: WaitContext; }

pub(crate) async fn with_quota_wait_context<F: Future>(
    store: &Store,
    session_id: &str,
    run_id: &str,
    future: F,
) -> F::Output {
    WAIT_CONTEXT
        .scope(
            WaitContext {
                store: store.clone(),
                session_id: session_id.into(),
                run_id: run_id.into(),
            },
            future,
        )
        .await
}

/// Persist a wait before sleeping. False means there is no owning harness turn;
/// errors (especially unresolved effects) must abort parking, not be ignored.
pub fn set_quota_wait(account: &str, window: &str, resume_at: &str, reason: &str) -> Result<bool> {
    WAIT_CONTEXT
        .try_with(|c| {
            let wait = QuotaWait {
                session_id: c.session_id.clone(),
                run_id: c.run_id.clone(),
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
        .try_with(|c| c.store.clear_quota_wait(&c.session_id, &c.run_id))
        .unwrap_or(Ok(false))
}

pub(crate) fn waiting_event(wait: &QuotaWait) -> EventKind {
    EventKind::RunWaitingForQuota { wait: wait.clone() }
}
