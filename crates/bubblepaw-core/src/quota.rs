//! Provider observations, independent of billing estimates and host routing policy.
use crate::{EventKind, Store};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::future::Future;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QuotaSource {
    ResponseHeaders,
    SdkEvent,
    UsageApi,
    Rejection,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct QuotaReading {
    pub provider: String,
    pub account: String,
    pub window: String,
    pub used_percent: f64,
    pub resets_at: Option<String>,
    pub observed_at: String,
    pub source: QuotaSource,
    /// Account credit balance known when this reading was recorded. Stored only
    /// as a column on change rows; a balance never writes on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance: Option<String>,
}

pub(crate) fn utc(value: &str) -> Result<()> {
    let time = chrono::DateTime::parse_from_rfc3339(value)?;
    ensure!(
        time.offset().local_minus_utc() == 0,
        "Timestamp must use UTC"
    );
    Ok(())
}

impl QuotaReading {
    pub fn validate(&self) -> Result<()> {
        for identity in [&self.provider, &self.account, &self.window] {
            ensure!(
                !identity.trim().is_empty() && identity.len() <= 256,
                "Quota identity must contain 1..=256 bytes"
            );
        }
        ensure!(
            self.used_percent.is_finite() && (0.0..=100.0).contains(&self.used_percent),
            "Quota used_percent must be finite and in 0..=100"
        );
        utc(&self.observed_at)?;
        if let Some(reset) = &self.resets_at {
            utc(reset)?;
        }
        Ok(())
    }
}

/// A restart hint, not a serialized continuation. In-process waits keep the
/// existing future; restart recovery admits an ordinary new run from history.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuotaWait {
    pub session_id: String,
    pub run_id: String,
    pub account: String,
    pub window: String,
    pub resume_at: String,
    pub reason: String,
}

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
impl QuotaWait {
    pub fn validate(&self) -> Result<()> {
        for identity in [&self.session_id, &self.run_id, &self.account, &self.window] {
            ensure!(
                !identity.trim().is_empty() && identity.len() <= 256,
                "Quota wait identity must contain 1..=256 bytes"
            );
        }
        ensure!(
            !self.reason.trim().is_empty() && self.reason.len() <= 4096,
            "Quota wait reason must contain 1..=4096 bytes"
        );
        utc(&self.resume_at)
    }
}

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
