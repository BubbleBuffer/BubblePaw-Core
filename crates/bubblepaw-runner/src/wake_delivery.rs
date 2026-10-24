//! Wake admission forwards structured reminders without rendering delivery identities.
use super::*;

impl WakeCoordinator {
    pub(super) fn deliver_ready(&self) -> Option<Duration> {
        let mut inbox = self.inbox.lock().expect("wake inbox poisoned");
        for (owner, generation) in
            std::mem::take(&mut *self.retired.lock().expect("wake retirement lock"))
        {
            if let Some(owner) = owner {
                if inbox
                    .callbacks
                    .get(&owner)
                    .is_some_and(|(current, _)| *current == generation)
                {
                    inbox.callbacks.remove(&owner);
                }
            } else if inbox
                .fallback
                .as_ref()
                .is_some_and(|(current, _)| *current == generation)
            {
                inbox.fallback = None;
            }
        }
        let optional_enabled = self.enabled.load(Ordering::Relaxed);
        let owners = inbox.notices.keys().cloned().collect::<Vec<_>>();
        let mut delay = None;
        for owner in owners {
            if inbox.suppressed.contains(&owner) {
                continue;
            }
            let notices = &inbox.notices[&owner];
            let Some(last) = notices
                .values()
                .filter(|notice| optional_enabled || notice.required)
                .map(|notice| notice.at)
                .max()
            else {
                continue;
            };
            let remaining = Duration::from_millis(self.debounce_ms.load(Ordering::Relaxed))
                .saturating_sub(last.elapsed());
            if !remaining.is_zero() {
                delay = Some(delay.map_or(remaining, |old: Duration| old.min(remaining)));
                continue;
            }
            let mut bytes = 0usize;
            let batch = notices
                .iter()
                .filter(|(_, notice)| optional_enabled || notice.required)
                .take_while(|(_, notice)| {
                    bytes += notice.reminder.text.len() + 1;
                    bytes <= 60 * 1024
                })
                .collect::<Vec<_>>();
            if batch.is_empty() {
                continue;
            }
            let keys = batch
                .iter()
                .map(|(key, _)| (*key).clone())
                .collect::<Vec<_>>();
            let reminders = batch
                .iter()
                .map(|(_, notice)| notice.reminder.clone())
                .collect::<Vec<_>>();
            let result = if let Some((_, callback)) = inbox.callbacks.get(&owner) {
                callback(reminders)
            } else if let Some((_, fallback)) = &inbox.fallback {
                fallback(&owner, reminders)
            } else {
                continue;
            };
            match result {
                Ok(_) => {
                    let notices = inbox.notices.get_mut(&owner).expect("wake owner");
                    for key in keys {
                        notices.remove(&key);
                    }
                    if !notices.is_empty() {
                        delay = Some(Duration::from_millis(50));
                    }
                }
                Err(error) => {
                    // Busy is normal: the active model request can still consume
                    // the inbox. Other transient admission failures also retain it.
                    let retry = if error.downcast_ref::<SessionBusy>().is_some() {
                        Duration::from_millis(50)
                    } else {
                        Duration::from_secs(1)
                    };
                    delay = Some(delay.map_or(retry, |old| old.min(retry)));
                }
            }
        }
        delay
    }
}
