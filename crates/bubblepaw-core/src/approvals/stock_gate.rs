//! The stock `ToolGate`: the approval policy, session rules and automatic review
//! behind the dispatcher's questions, and the host's result hook after a call.
use super::{ApprovalPolicy, AutoApprovalConfig, AutoApprovalContext, Gate};
use crate::tool_gate::{GateCall, GateSource, ToolGate, Verdict};
use crate::{ApprovalMode, Store, ToolDefinition, ToolResult, WorkspaceResolver};
use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;

pub(crate) struct StockGate {
    store: Store,
    auto_approval: Option<AutoApprovalConfig>,
    workspace_resolver: Option<WorkspaceResolver>,
}

impl StockGate {
    pub(crate) fn new(
        store: Store,
        auto_approval: Option<AutoApprovalConfig>,
        workspace_resolver: Option<WorkspaceResolver>,
    ) -> Self {
        Self {
            store,
            auto_approval,
            workspace_resolver,
        }
    }
    /// Directories a session's exec calls run in, first the directory they start
    /// from, then its root chat's workspace. Other chats' roots never become
    /// protected roots for this session.
    fn workspace_roots(&self, session_id: &str) -> Result<Vec<PathBuf>> {
        let mut roots = Vec::new();
        if let Some(root) = self
            .workspace_resolver
            .as_ref()
            .and_then(|resolve| resolve(session_id))
        {
            roots.push(root);
        }
        if let Some(bound) = self.store.session_workspace(session_id)?
            && !roots.contains(&bound)
        {
            roots.push(bound);
        }
        Ok(roots)
    }
}

#[async_trait]
impl ToolGate for StockGate {
    fn admits_without_write(
        &self,
        call: &GateCall<'_>,
        definition: &ToolDefinition,
    ) -> Result<bool> {
        let policy = ApprovalPolicy::of(call.context);
        let workspaces = self.workspace_roots(&call.context.session_id)?;
        Ok(
            policy.mode(&call.call.name, definition.approval) == ApprovalMode::Allow
                && policy
                    .hardline_match(&call.call.name, &call.call.arguments, &workspaces)
                    .is_none(),
        )
    }
    async fn before(&self, call: &GateCall<'_>, definition: &ToolDefinition) -> Result<Verdict> {
        let workspaces = self.workspace_roots(&call.context.session_id)?;
        let auto = self
            .auto_approval
            .as_ref()
            .map(|config| AutoApprovalContext {
                config: config.clone(),
                user_request: call.user_request.to_owned(),
            });
        let gate = super::gate(
            &self.store,
            ApprovalPolicy::of(call.context),
            auto.as_ref(),
            call.context,
            &workspaces,
            super::CallSource {
                call: call.call,
                message_id: match call.source {
                    GateSource::Model { message_id } => Some(message_id),
                    GateSource::Nested => None,
                },
                interactive: call.interactive,
            },
            definition,
        )
        .await?;
        Ok(match gate {
            Gate::Proceed => Verdict::Allow,
            Gate::Denied(message) => Verdict::Deny(message),
            Gate::Cancelled => Verdict::Cancelled,
        })
    }
    fn after(&self, call: &GateCall<'_>, result: ToolResult) -> Result<ToolResult> {
        match call.result_hook {
            Some(limit) => limit(call.context, result, call.result_limit),
            None => Ok(result),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ContextStrategy;
    use std::sync::Arc;

    #[test]
    fn protected_roots_follow_the_session_and_its_worktree_only() -> Result<()> {
        let store = Store::in_memory()?;
        let a_directory = tempfile::tempdir()?;
        let a = a_directory.path().canonicalize()?;
        let b_directory = tempfile::tempdir()?;
        let b = b_directory.path().canonicalize()?;
        let worktree_directory = tempfile::tempdir()?;
        let worktree = worktree_directory.path().canonicalize()?;
        store.bind_workspace(&a)?;
        let root_a = store.create_session_with_workspace("a", &a)?;
        let root_b = store.create_session_with_workspace("b", &b)?;
        let child = store.create_session_with_parent(
            "child",
            ContextStrategy::default(),
            Some(&root_b.id),
        )?;
        let gate = StockGate::new(store.clone(), None, None);
        assert_eq!(gate.workspace_roots(&root_a.id)?, vec![a.clone()]);
        assert_eq!(gate.workspace_roots(&root_b.id)?, vec![b.clone()]);
        let child_id = child.id.clone();
        let effective = worktree.clone();
        let gate = StockGate::new(
            store,
            None,
            Some(Arc::new(move |session| {
                (session == child_id).then(|| effective.clone())
            })),
        );
        let roots = gate.workspace_roots(&child.id)?;
        assert_eq!(roots, vec![worktree.clone(), b.clone()]);
        assert!(!gate.workspace_roots(&child.id)?.contains(&a));
        let policy = crate::ApprovalPolicy::default();
        for root in [worktree, b] {
            assert!(
                policy
                    .hardline_match(
                        "exec",
                        &serde_json::json!({"command": format!("rm -rf '{}'", root.display())}),
                        &roots
                    )
                    .is_some()
            );
        }
        assert!(
            policy
                .hardline_match(
                    "exec",
                    &serde_json::json!({"command": format!("rm -rf '{}'", a.display())}),
                    &roots
                )
                .is_none()
        );
        Ok(())
    }
}
