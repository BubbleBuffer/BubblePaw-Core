//! Host notice and cancellation collection.
use crate::reminder_types::Reminder;
use crate::tool_registry::ToolRegistry;

impl ToolRegistry {
    pub fn take_reminders(&self, session: &str) -> Vec<Reminder> {
        self.tools
            .values()
            .chain(self.host_tools.values())
            .flat_map(|tool| tool.take_reminders(session))
            .collect()
    }
    pub fn cancel_session(&self, session: &str) -> usize {
        self.tools
            .values()
            .chain(self.host_tools.values())
            .map(|tool| tool.cancel_session(session))
            .sum()
    }
}
