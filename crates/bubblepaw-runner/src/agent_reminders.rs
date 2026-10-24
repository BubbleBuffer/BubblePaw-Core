//! Child reminders retain typed session/run identities for restart recovery.
use super::*;
use bubblepaw_core::Reminder;

pub(super) fn child_delivery_id(session: &str, run: &str) -> String {
    serde_json::to_string(&[session, run]).expect("child identity is JSON")
}
pub(super) fn child_completion(session: &str, run: &str, text: String) -> Reminder {
    Reminder::new(format!("child_completion:{session}:{run}"), text)
        .with_delivery("child_completion", child_delivery_id(session, run))
}
impl AgentTools {
    /// Drain direct-child completion notices and collect due stale-child reminders.
    pub fn take_reminders(&self, owner: &str) -> Vec<Reminder> {
        let mut notices = self.registry.wake.take_reminders(owner, "child:");
        if let Ok(recovered) = self.recovered_reminders(owner) {
            notices.extend(recovered);
        }
        notices.extend(self.take_reminders_at(owner, Instant::now()));
        notices
    }
    fn recovered_reminders(&self, owner: &str) -> Result<Vec<Reminder>> {
        let retained = {
            let mut state = self
                .registry
                .state
                .lock()
                .map_err(|_| anyhow!("agent registry poisoned"))?;
            if !state.recovery_checked.insert(owner.to_owned()) {
                return Ok(Vec::new());
            }
            state
                .children
                .values()
                .map(|child| child.session_id.clone())
                .collect::<HashSet<_>>()
        };
        let recover = || -> Result<Vec<Reminder>> {
            let mut candidates = Vec::new();
            for session in self.store.list_sessions()?.into_iter().filter(|session| {
                !session.hidden
                    && session.parent_session_id.as_deref() == Some(owner)
                    && !retained.contains(&session.id)
            }) {
                let Some(run) = self.store.runs(&session.id)?.into_iter().last() else {
                    continue;
                };
                if run.ended_at.is_none()
                    || !matches!(run.status.as_str(), "completed" | "cancelled" | "failed")
                    || self
                        .store
                        .attachment(&session.id, &format!("{READ_PREFIX}{}", run.id))?
                        .is_some()
                {
                    continue;
                }
                let legacy = format!("runId {})", run.id);
                let mut reply = terminal_reply("restored", &run);
                if let Some((metadata, _)) = self
                    .store
                    .attachment(&session.id, &format!("{FAILURE_PREFIX}{}", run.id))?
                {
                    reply = serde_json::from_str(&metadata)?;
                }
                let text = reply["text"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(4000)
                    .collect::<String>();
                candidates.push((
                    crate::wake::DeliveryMarker::new(
                        "child_completion",
                        child_delivery_id(&session.id, &run.id),
                        legacy,
                    ),
                    child_completion(
                        &session.id,
                        &run.id,
                        format!(
                            "Child (sessionId {}, runId {}) ended with status {}.\n{}",
                            session.id,
                            run.id,
                            reply["status"].as_str().unwrap_or("failed"),
                            text
                        ),
                    ),
                ));
            }
            if candidates.is_empty() {
                return Ok(Vec::new());
            }
            let pending = crate::wake::undelivered_markers(
                &self.store,
                owner,
                candidates.iter().map(|(marker, _)| {
                    crate::wake::DeliveryMarker::new(
                        marker.kind,
                        marker.id.clone(),
                        marker.legacy.clone(),
                    )
                }),
            )?;
            Ok(candidates
                .into_iter()
                .filter(|(marker, _)| pending.contains(&marker.id))
                .map(|(_, text)| text)
                .collect())
        };
        let result = recover();
        if result.is_err() {
            self.registry
                .state
                .lock()
                .map_err(|_| anyhow!("agent registry poisoned"))?
                .recovery_checked
                .remove(owner);
        }
        result
    }
    pub(super) fn take_reminders_at(&self, owner: &str, now: Instant) -> Vec<Reminder> {
        let interval = Duration::from_secs(self.limits().stale_reminder_secs);
        if interval.is_zero() {
            return Vec::new();
        }
        let Ok(state) = self.registry.state.lock() else {
            return Vec::new();
        };
        let mut children = state
            .children
            .values()
            .filter(|child| child.owner == owner && !child.hidden)
            .collect::<Vec<_>>();
        children.sort_by(|a, b| a.handle.cmp(&b.handle));
        children.into_iter().filter_map(|child| {
            let mut turn = child.turn.lock().ok()?;
            let last = turn.last_reminder.unwrap_or(turn.last_interaction);
            if turn.result.is_some() || turn.active_waits > 0 || now.saturating_duration_since(last) < interval { return None; }
            turn.last_reminder = Some(now);
            Some(Reminder::new(format!("child_stale:{}", child.session_id), format!("Child {} (sessionId {}) has been running for {} seconds and has had no parent interaction for {} seconds. Check agent_status, wait, send guidance, or cancel it.", child.handle, child.session_id, now.saturating_duration_since(turn.started).as_secs(), now.saturating_duration_since(turn.last_interaction).as_secs())).with_delivery("child_stale", child.session_id.clone()))
        }).collect()
    }
}
