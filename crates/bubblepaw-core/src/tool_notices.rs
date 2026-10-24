//! Host notice and cancellation collection.
use crate::*;
impl ToolRegistry {
    pub(crate) fn take_reminders(&self, session: &str) -> Vec<Reminder> {
        self.tools
            .values()
            .chain(self.host_tools.values())
            .flat_map(|tool| tool.take_reminders(session))
            .collect()
    }
    pub(crate) fn cancel_session(&self, session: &str) -> usize {
        self.tools
            .values()
            .chain(self.host_tools.values())
            .map(|tool| tool.cancel_session(session))
            .sum()
    }
}
