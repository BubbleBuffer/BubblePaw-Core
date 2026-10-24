//! Known process outcomes retain their handle independently of rendered text.
use super::*;

pub(super) fn process_completion(handle: &str, text: String) -> bubblepaw_core::Reminder {
    bubblepaw_core::Reminder::new(format!("process_completion:{handle}"), text)
        .with_delivery("process_completion", handle.to_owned())
}
impl ProcessTools {
    pub fn take_reminders(&self, owner: &str) -> Vec<bubblepaw_core::Reminder> {
        let mut notices = self.wake_coordinator().take_reminders(owner, "process:");
        if let Ok(recovered) = self.recovered_reminders(owner) {
            notices.extend(recovered);
        }
        notices
    }
    fn recovered_reminders(&self, owner: &str) -> Result<Vec<bubblepaw_core::Reminder>> {
        let Some(store) = self.store.get() else {
            return Ok(Vec::new());
        };
        if !self
            .recovered
            .lock()
            .map_err(|_| anyhow!("process recovery lock"))?
            .insert(owner.to_owned())
        {
            return Ok(Vec::new());
        }
        let recover = || -> Result<Vec<bubblepaw_core::Reminder>> {
            let live = self
                .sessions
                .lock()
                .map_err(|_| anyhow!("process registry poisoned"))?
                .values()
                .filter(|session| session.owner == owner)
                .map(|session| session.handle.clone())
                .collect::<std::collections::HashSet<_>>();
            let mut after = None;
            let mut candidates = Vec::new();
            loop {
                let page = store.attachment_metadata_page(
                    owner,
                    "bubblepaw-process-notice-v1:",
                    after.as_deref(),
                    128,
                )?;
                if page.is_empty() {
                    break;
                }
                for (_, metadata) in &page {
                    let value: Value = serde_json::from_str(metadata)?;
                    let handle = value["handle"]
                        .as_str()
                        .ok_or_else(|| anyhow!("invalid process notice handle"))?;
                    if live.contains(handle) {
                        continue;
                    }
                    if let Some(notice) = value["notice"].as_str() {
                        candidates.push((handle.to_owned(), notice.to_owned()));
                    }
                }
                after = page.last().map(|(id, _)| id.clone());
            }
            if candidates.is_empty() {
                return Ok(Vec::new());
            }
            let pending = crate::wake::undelivered_markers(
                store,
                owner,
                candidates.iter().map(|(handle, _)| {
                    crate::wake::DeliveryMarker::new(
                        "process_completion",
                        handle.clone(),
                        handle.clone(),
                    )
                }),
            )?;
            Ok(candidates
                .into_iter()
                .filter(|(handle, _)| pending.contains(handle))
                .map(|(handle, notice)| process_completion(&handle, notice))
                .collect())
        };
        let result = recover();
        if result.is_err() {
            self.recovered
                .lock()
                .map_err(|_| anyhow!("process recovery lock"))?
                .remove(owner);
        }
        result
    }
}
