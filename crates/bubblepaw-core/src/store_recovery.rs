//! Startup recovery preserves canonical writes before publishing parent state.
use super::*;
use crate::event_log::{PendingEvent, parent_run_state_event};
use std::sync::atomic::AtomicBool;

#[derive(Default)]
pub(super) struct RecoveryEvents {
    ready: AtomicBool,
    pending: Mutex<Vec<PendingEvent>>,
}

impl Store {
    pub(super) fn finish_chat_open_recovery(&self, split: bool) -> Result<()> {
        if split {
            self.recover()?;
        }
        // Recovery ran against the record before opening any serving projection.
        // Expired/completed metadata can now move to its proper lifetime.
        self.archive_operations()
    }

    pub(super) fn recover(&self) -> Result<()> {
        let interrupted: Vec<Run> = {
            let conn = self.0.connection.lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT data FROM runs WHERE status IN ('running','waiting_for_quota')")?;
            stmt.query_map([], |r| r.get::<_, String>(0))?
                .map(|r| Ok(serde_json::from_str(&r?)?))
                .collect::<Result<_>>()?
        };
        for mut run in interrupted {
            // Waiting is only an automatic recovery hint while no external
            // outcome is unresolved. Keep the marker for host startup policy.
            self.close_unresolved(&run.session_id, &run.id)?;
            run.status = "interrupted".into();
            run.error = Some("Process stopped before run completion".into());
            run.ended_at = Some(now());
            self.finish_recovered_run(&run)?;
        }
        self.recover_quota_waits()?;
        crate::approvals::recover(self)?;
        let pending: bool = self.0.connection.lock().unwrap().query_row(
            "SELECT EXISTS(SELECT 1 FROM effects WHERE state='dispatched')",
            [],
            |r| r.get(0),
        )?;
        if pending {
            self.write(|tx| {
                tx.execute(
                    "UPDATE effects SET state='unknown' WHERE state='dispatched'",
                    [],
                )?;
                Ok(())
            })?;
        }
        Ok(())
    }

    fn finish_recovered_run(&self, run: &Run) -> Result<()> {
        let event =
            self.read(|tx| parent_run_state_event(tx, &run.session_id, &run.id, &run.status))?;
        let Some(event) = event else {
            return self.finish_run(run);
        };
        let Some(host) = self.host() else {
            // Legacy single-file stores have no startup reservation/reset step.
            // Keep the parent event atomic with the canonical interrupted state.
            return self
                .runtime()
                .log
                .commit_targeted(self, &run.session_id, |tx| {
                    tx.execute(
                        "UPDATE runs SET status=?2,data=?3 WHERE id=?1",
                        params![run.id, run.status, Self::run_metadata(tx, run)?],
                    )?;
                    Ok(((), vec![event]))
                });
        };
        self.finish_run(run)?;
        let recovery = &host.0.layout.as_ref().unwrap().recovery;
        if !recovery.ready.load(Ordering::Acquire) {
            recovery.pending.lock().unwrap().push(event);
            return Ok(());
        }
        host.publish_recovered_parent_event(event)
    }

    pub(super) fn publish_startup_recovery_events(&self) -> Result<()> {
        // The catalog and sequence reservation must already be initialized.
        // A failed open may lose this disposable queue; the next directory
        // startup advances the reservation and forces stale clients to reset.
        let recovery = &self.0.layout.as_ref().unwrap().recovery;
        recovery.ready.store(true, Ordering::Release);
        let pending = std::mem::take(&mut *recovery.pending.lock().unwrap());
        for event in pending {
            self.publish_recovered_parent_event(event)?;
        }
        Ok(())
    }

    fn publish_recovered_parent_event(&self, event: PendingEvent) -> Result<()> {
        let session = event.session_id.clone();
        self.runtime()
            .log
            .commit_targeted(self, &session, |_| Ok(((), vec![event])))
    }
}

#[cfg(test)]
#[path = "store_recovery_tests.rs"]
mod tests;
