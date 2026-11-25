use super::{Approval, ApprovalMode, ApprovalPolicy};
use crate::{ApprovalResolver, HarnessConfig, Store};
use anyhow::Result;
use rusqlite::params;
use std::sync::Arc;

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

    /// Combine approval rules with a run or child restriction.
    pub fn tightened_by(&self, restriction: &Self) -> Self {
        let tools = self
            .tools
            .keys()
            .chain(restriction.tools.keys())
            .map(|name| {
                // A named rule can override the default for confirming tools.
                let mode = stricter(
                    self.mode(name, Approval::Confirm),
                    restriction.mode(name, Approval::Confirm),
                );
                (name.clone(), mode)
            })
            .collect();
        Self {
            confirm: stricter(self.confirm, restriction.confirm),
            tools,
            deny: restriction.clone().with_inherited_denies(self).deny,
            timeout_secs: self.timeout_secs,
            hardline: self.hardline,
        }
    }
}

fn stricter(a: ApprovalMode, b: ApprovalMode) -> ApprovalMode {
    fn rank(mode: ApprovalMode) -> u8 {
        match mode {
            ApprovalMode::Deny => 0,
            ApprovalMode::Ask => 1,
            ApprovalMode::Auto => 2,
            ApprovalMode::Allow => 3,
        }
    }
    if rank(a) <= rank(b) { a } else { b }
}

impl HarnessConfig {
    pub(crate) fn run_approval_resolver(
        &self,
        requested: Option<ApprovalPolicy>,
        single_tool: bool,
    ) -> Option<ApprovalResolver> {
        let resolve = self.approval_resolver.clone();
        let Some(requested) = requested else {
            return resolve;
        };
        let baseline = self.approval.clone();
        Some(Arc::new(move |session: &str| {
            let current = resolve
                .as_ref()
                .map_or_else(|| baseline.clone(), |resolve| resolve(session));
            if single_tool {
                // The model-free call_tool API accepts explicit authorization.
                return requested
                    .clone()
                    .with_inherited_denies(&current)
                    .with_inherited_hardline(&current);
            }
            ApprovalPolicy {
                timeout_secs: requested.timeout_secs,
                ..current.tightened_by(&requested)
            }
        }))
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
