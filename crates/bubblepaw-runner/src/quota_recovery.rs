//! Startup-only recovery of durable quota waits. It does not poll providers.
use anyhow::Result;
use bubblepaw_core::{Harness, QuotaWait, QuotaWaitChanged, Run, RunOptions, SessionBusy};
use chrono::Utc;
use std::{sync::Arc, time::Duration};
use tokio::task::JoinHandle;

pub type ResumeQuota = Arc<dyn Fn(&QuotaWait) -> Result<(Run, Arc<Harness>)> + Send + Sync>;

pub fn root_resume(harness: Arc<Harness>) -> ResumeQuota {
    root_resume_scoped(harness, crate::session_scope::SessionScope::all())
}

pub fn root_resume_scoped(
    harness: Arc<Harness>,
    scope: crate::session_scope::SessionScope,
) -> ResumeQuota {
    Arc::new(move |wait| {
        let previous = harness.store().run(&wait.run_id)?;
        let run = scope.admit_owned(harness.store(), &wait.session_id, || {
            harness.start_quota_resume(
                wait,
                RunOptions {
                    model: Some(previous.model),
                    ..Default::default()
                },
            )
        })?;
        Ok((run, harness.clone()))
    })
}

/// Join handles are owned by the host and aborted/joined before host shutdown.
/// A hard interruption leaves the marker; ordinary cancellation clears it.
pub struct QuotaRecovery {
    tasks: Vec<JoinHandle<()>>,
}
impl QuotaRecovery {
    pub fn start(harness: Arc<Harness>, resume: ResumeQuota) -> Result<Self> {
        Self::start_scoped(harness, resume, crate::session_scope::SessionScope::all())
    }

    pub fn start_scoped(
        harness: Arc<Harness>,
        resume: ResumeQuota,
        scope: crate::session_scope::SessionScope,
    ) -> Result<Self> {
        let mut tasks = Vec::new();
        for wait in harness.store().pending_quota_waits()? {
            if !scope.contains(harness.store(), &wait.session_id) {
                continue;
            }
            let harness = harness.clone();
            let resume = resume.clone();
            let scope = scope.clone();
            tasks.push(tokio::spawn(async move {
                let reset = match chrono::DateTime::parse_from_rfc3339(&wait.resume_at) {
                    Ok(reset) => reset,
                    Err(error) => {
                        fail(&harness, &wait, &format!("Invalid quota reset: {error}"));
                        return;
                    }
                };
                tokio::time::sleep(
                    reset
                        .signed_duration_since(Utc::now())
                        .to_std()
                        .unwrap_or_default(),
                )
                .await;
                loop {
                    if !scope.contains(harness.store(), &wait.session_id) {
                        return;
                    }
                    match current(&harness, &wait) {
                        Ok(false) => return,
                        Err(error) => {
                            tracing::warn!(%error, "Could not inspect quota recovery marker");
                            return;
                        }
                        Ok(true) => {}
                    }
                    match resume(&wait) {
                        Ok(_) => return,
                        Err(error)
                            if error
                                .downcast_ref::<crate::session_scope::SessionLeaseBusy>()
                                .is_some() =>
                        {
                            return;
                        }
                        Err(error) if error.downcast_ref::<QuotaWaitChanged>().is_some() => return,
                        Err(error) if error.downcast_ref::<SessionBusy>().is_some() => {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                        Err(error) => {
                            fail(&harness, &wait, &format!("Quota resume failed: {error}"));
                            return;
                        }
                    }
                }
            }));
        }
        Ok(Self { tasks })
    }

    pub async fn shutdown(self) {
        for task in &self.tasks {
            task.abort();
        }
        for task in self.tasks {
            let _ = task.await;
        }
    }
}

/// A one-shot host reports future waits and completes due recoveries before its
/// requested command. It does not keep itself alive solely for future markers.
pub async fn resume_due_once(
    harness: &Arc<Harness>,
    resume: &ResumeQuota,
    explicit_resume_session: Option<&str>,
) -> Result<usize> {
    let waits = harness.store().pending_quota_waits()?;
    let mut resumed = 0;
    for wait in waits {
        eprintln!(
            "Pending quota wait: session {}, account {}, window {}, resumes {}",
            wait.session_id, wait.account, wait.window, wait.resume_at
        );
        // The requested resume command itself consumes this marker; starting it
        // here too would cause two model turns for a single CLI command.
        if explicit_resume_session == Some(wait.session_id.as_str()) {
            continue;
        }
        let reset = chrono::DateTime::parse_from_rfc3339(&wait.resume_at)?;
        if reset > Utc::now() || !current(harness, &wait)? {
            continue;
        }
        match resume(&wait) {
            Ok((_, owner)) => {
                owner.wait_for_idle(&wait.session_id).await?;
                resumed += 1;
            }
            Err(error) if error.downcast_ref::<QuotaWaitChanged>().is_some() => {}
            Err(error) if error.downcast_ref::<SessionBusy>().is_some() => {}
            Err(error) => fail(harness, &wait, &format!("Quota resume failed: {error}")),
        }
    }
    Ok(resumed)
}

fn current(harness: &Harness, wait: &QuotaWait) -> Result<bool> {
    Ok(harness.store().quota_wait(&wait.session_id)?.as_ref() == Some(wait))
}

fn fail(harness: &Harness, wait: &QuotaWait, reason: &str) {
    tracing::warn!(session = %wait.session_id, account = %wait.account, %reason, "Quota recovery stopped");
    if let Err(error) = harness
        .store()
        .fail_quota_wait(&wait.session_id, &wait.run_id, reason)
    {
        tracing::error!(%error, "Failed to clear unsafe quota recovery marker");
    }
}
