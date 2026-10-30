//! Live config changes: typed changes share the reminder queue; root access follows read-only.
use super::*;

impl LiveConfig {
    /// Begin observing host config changes without replaying older reloads.
    pub(crate) fn initialize_config_cursor(&self, session: &str) {
        let mut state = self.state.lock().unwrap();
        let through = state.notices.len();
        state.consumed.entry(session.into()).or_insert(through);
    }

    pub fn take_reminders(&self, session: &str) -> Vec<bubblepaw_core::Reminder> {
        let mut reminders = Vec::new();
        for (kind, notices) in [
            ("memory_files", self.memory.take_changes(session)),
            ("skills", self.skills.take_changes(session)),
        ] {
            if !notices.is_empty() {
                reminders.push(change_reminder(kind, notices.join("\n")));
            }
        }
        if let Some(notice) = self.take_config_notice(session) {
            reminders.push(change_reminder("live_config", notice));
        }
        reminders
    }
    fn take_config_notice(&self, session: &str) -> Option<String> {
        let mut state = self.state.lock().unwrap();
        let through = state.notices.len();
        let start = state
            .consumed
            .insert(session.into(), through)
            .unwrap_or(through);
        if start == through {
            return None;
        }
        let mut notice = state.notices[start..through].join("\n");
        if notice.len() > 4096 {
            let mut end = 4050;
            while !notice.is_char_boundary(end) {
                end -= 1;
            }
            notice.truncate(end);
            notice.push_str("; further changes omitted (see reload result)");
        }
        Some(notice)
    }
}

// These are incremental change events, so independent batches cannot share a
// snapshot kind: the queue's latest-per-kind rule would discard earlier deltas.
fn change_reminder(family: &str, text: String) -> bubblepaw_core::Reminder {
    // A later recurrence with identical text is still a new change event.
    bubblepaw_core::Reminder::new(format!("{family}:{}", uuid::Uuid::new_v4()), text)
}

fn root_access_line(read_only: bool) -> &'static str {
    if read_only {
        "Workspace access: read-only (no commands or writes)."
    } else {
        "Workspace access: read/write."
    }
}

impl LiveConfig {
    /// The session's system prompt. Root sessions get the access sentence from the
    /// live read-only setting, before the memory files; children keep the static
    /// access text in their own prompts.
    pub(crate) fn system_prompt_base(&self, session: &str, base: &str) -> Result<String> {
        self.initialize_config_cursor(session);
        let base = if self.store.session(session)?.parent_session_id.is_some() {
            base.to_owned()
        } else {
            let read_only = self.providers.snapshot().settings.approvals.read_only;
            format!("{base} {}", root_access_line(read_only))
        };
        Ok(base)
    }
}
