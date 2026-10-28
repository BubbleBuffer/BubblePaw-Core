//! Atomic companion host state for hosts that project a source into another session.
use super::*;
use crate::event_log::PendingEvent;

impl Store {
    /// Update a value and one host-supplied session projection. When both
    /// sessions share a canonical database, both rows and their events commit
    /// together. Otherwise the source commits first and one target transaction
    /// follows; the caller can repair that disposable projection after a crash.
    /// The source session's own state is never overwritten by its projection.
    pub fn update_host_state_with_projection(
        &self,
        session_id: &str,
        run_id: &str,
        key: &str,
        projection: (&str, &str),
        update: impl FnOnce(Option<Value>) -> Result<(Value, Value)>,
    ) -> Result<Value> {
        let (target, target_key) = projection;
        for key in [key, target_key] {
            ensure!(
                !key.is_empty() && key.len() <= 64,
                "Host state key must be 1..64 bytes"
            );
        }
        ensure!(
            session_id != target || key != target_key,
            "Projection must use a different host state key"
        );
        ensure!(
            self.active_session_metadata(target)?.is_some(),
            "Projection session is not active"
        );
        let mut deferred = None;
        let value = self.runtime().log.commit_targeted(self, session_id, |tx| {
            let previous: Option<String> = tx
                .query_row(
                    "SELECT value FROM host_state WHERE session_id=?1 AND key=?2",
                    params![session_id, key],
                    |row| row.get(0),
                )
                .optional()?;
            let previous = previous
                .map(|value| serde_json::from_str(&value))
                .transpose()?;
            let (value, projected) = update(previous)?;
            // Validate the companion before the source write, including the
            // fallback case where its canonical database differs.
            ensure!(
                serde_json::to_vec(&projected)?.len() <= MAX_STATE_BYTES,
                "Host state exceeds 64 KiB"
            );
            Self::write_host_state_tx(tx, session_id, key, &value, false)?;
            let mut events = vec![PendingEvent::new(
                session_id,
                run_id,
                EventKind::HostStateChanged {
                    key: key.into(),
                    value: value.clone(),
                },
            )];
            let shared: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
                [target],
                |row| row.get(0),
            )?;
            if shared {
                Self::write_host_state_tx(tx, target, target_key, &projected, false)?;
                events.push(PendingEvent::new(
                    target,
                    run_id,
                    EventKind::HostStateChanged {
                        key: target_key.into(),
                        value: projected,
                    },
                ));
            } else {
                deferred = Some(projected);
            }
            Ok((value, events))
        })?;
        if let Some(projected) = deferred {
            self.update_host_state(target, run_id, target_key, |_| Ok(projected))?;
        }
        Ok(value)
    }
}

#[cfg(test)]
#[path = "host_state_projection_tests.rs"]
mod tests;
