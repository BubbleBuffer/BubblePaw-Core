use super::ApprovalPolicy;
use crate::Store;
use anyhow::Result;
use rusqlite::params;

impl ApprovalPolicy {
    /// Same grammar as session allowances: one exact tool name, without globs
    /// or argument predicates. Return the first matching configured rule.
    pub fn denied_rule(&self, tool: &str) -> Option<&str> {
        self.deny
            .iter()
            .find(|rule| rule.as_str() == tool)
            .map(String::as_str)
    }

    /// A run override or child profile can add denies but cannot drop its host's.
    pub fn with_inherited_denies(mut self, parent: &Self) -> Self {
        for rule in &parent.deny {
            if !self.deny.contains(rule) {
                self.deny.push(rule.clone());
            }
        }
        self
    }
}

pub(super) fn has_rule(store: &Store, session_id: &str, tool: &str) -> Result<bool> {
    if let Some(chat) = store.scoped(session_id)? {
        return has_rule(&chat, session_id, tool);
    }
    store.read(|conn| {
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM approval_rules WHERE session_id=?1 AND tool=?2)",
            params![session_id, tool],
            |r| r.get(0),
        )?)
    })
}
