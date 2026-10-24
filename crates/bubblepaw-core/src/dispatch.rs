use crate::{
    CancellationToken, EventKind, Observation, ObservationScope, OutputRecord, Store, ToolCall,
    ToolContext, ToolRegistry, ToolResult, id, now,
};
use anyhow::{Result, anyhow, ensure};
use serde_json::Value;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

mod calls;

/// One dispatch path for direct model calls and host calls composed inside a cell.
/// Nested reads stay transient. Nested effects retain boundaries, without inventing
/// provider tool messages for calls that the provider did not make.
#[derive(Clone)]
pub struct ToolDispatcher {
    store: Store,
    tools: ToolRegistry,
    log: Arc<crate::event_log::Log>,
    max_result_bytes: usize,
    observation: Option<Observation>,
    scope: ObservationScope,
    approval: Arc<crate::ApprovalPolicy>,
    approval_resolver: Option<crate::ApprovalResolver>,
    workspace_resolver: Option<crate::WorkspaceResolver>,
    auto_approval: Option<crate::approvals::AutoApprovalContext>,
    remaining_calls: Option<Arc<AtomicUsize>>,
    refresh_tools: Arc<AtomicBool>,
    interactive_approval: bool,
    origin: &'static str,
    pub(crate) submitted_result: Arc<std::sync::Mutex<Option<Value>>>,
}

impl ToolDispatcher {
    /// Canonical host storage for immutable tool attachments and metadata.
    pub fn store(&self) -> &Store {
        &self.store
    }
    pub(crate) fn new(
        store: Store,
        tools: ToolRegistry,
        max_result_bytes: usize,
        observation: Option<Observation>,
    ) -> Self {
        let log = store.runtime().log.clone();
        Self {
            store,
            tools,
            log,
            max_result_bytes,
            observation,
            scope: ObservationScope::default(),
            approval: Arc::default(),
            approval_resolver: None,
            workspace_resolver: None,
            auto_approval: None,
            remaining_calls: None,
            refresh_tools: Arc::default(),
            interactive_approval: true,
            origin: "model",
            submitted_result: Arc::default(),
        }
    }
    pub(crate) fn with_tool_budget(mut self, limit: Option<usize>) -> Self {
        self.remaining_calls = limit.map(|limit| Arc::new(AtomicUsize::new(limit)));
        self
    }
    pub(crate) fn with_interactive_approval(mut self, enabled: bool) -> Self {
        self.interactive_approval = enabled;
        self.origin = if enabled { "model" } else { "embedder" };
        self
    }
    /// Reserve before approval and durable dispatch. Clones share the same run budget.
    #[allow(deprecated)] // fetch_update supports the workspace's Rust 1.88 MSRV.
    pub(crate) fn reserve_tool_call(&self) -> bool {
        self.remaining_calls.as_ref().is_none_or(|remaining| {
            remaining
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                    value.checked_sub(1)
                })
                .is_ok()
        })
    }
    /// Remaining calls in this run, after reservations already admitted.
    pub fn remaining_tool_calls(&self) -> Option<usize> {
        self.remaining_calls
            .as_ref()
            .map(|remaining| remaining.load(Ordering::Relaxed))
    }
    pub(crate) fn with_approval(mut self, policy: crate::ApprovalPolicy) -> Self {
        self.approval = Arc::new(policy);
        self
    }
    pub(crate) fn with_approval_resolver(
        mut self,
        resolve: Option<crate::ApprovalResolver>,
    ) -> Self {
        self.approval_resolver = resolve;
        self
    }
    pub(crate) fn with_workspace_resolver(
        mut self,
        resolve: Option<crate::WorkspaceResolver>,
    ) -> Self {
        self.workspace_resolver = resolve;
        self
    }
    /// Directories a session's exec calls run in, first the directory they start
    /// from, then its root chat's workspace. Other chats' roots never become
    /// protected roots for this session.
    pub(crate) fn workspace_roots(&self, session_id: &str) -> Result<Vec<std::path::PathBuf>> {
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
    pub(crate) fn approval_policy(&self, session_id: &str) -> crate::ApprovalPolicy {
        self.approval_resolver
            .as_ref()
            .map(|resolve| resolve(session_id))
            .unwrap_or_else(|| (*self.approval).clone())
    }
    /// Inherit this call's approval scope, preserving an explicit run override.
    /// A host may further restrict the returned policy for a child session.
    pub fn approval_resolver(&self, session_id: &str) -> crate::ApprovalResolver {
        let resolve = self.approval_resolver.clone();
        let policy = self.approval.clone();
        let session_id = session_id.to_owned();
        Arc::new(move |_| {
            resolve
                .as_ref()
                .map(|resolve| resolve(&session_id))
                .unwrap_or_else(|| (*policy).clone())
        })
    }
    /// Request a fresh host registry at the next model round of this turn only.
    /// Existing calls and clones retain their snapshot until that boundary.
    pub fn refresh_tools_next_round(&self) {
        self.refresh_tools.store(true, Ordering::Release);
    }
    pub(crate) fn take_tools_refresh(&self) -> bool {
        self.refresh_tools.swap(false, Ordering::AcqRel)
    }
    pub(crate) fn with_tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = tools;
        self
    }
    pub(crate) fn with_auto_approval(
        mut self,
        config: Option<crate::AutoApprovalConfig>,
        user_request: String,
    ) -> Self {
        self.auto_approval = config.map(|config| crate::approvals::AutoApprovalContext {
            config,
            user_request,
        });
        self
    }
    /// The approval gate for a validated call, before its effect boundary.
    async fn approve_source(
        &self,
        context: &ToolContext,
        call: &ToolCall,
        message_id: Option<&str>,
    ) -> Result<crate::approvals::Gate> {
        let definition = self
            .tools
            .get(&call.name)
            .ok_or_else(|| anyhow!("Unknown tool: {}", call.name))?
            .definition();
        let workspaces = self.workspace_roots(&context.session_id)?;
        crate::approvals::gate(
            &self.store,
            &context.approval,
            self.auto_approval.as_ref(),
            context,
            &workspaces,
            crate::approvals::CallSource {
                call,
                message_id,
                interactive: self.interactive_approval,
            },
            &definition,
        )
        .await
    }
    /// Attribute host calls to the JS cell that owns them, including background work.
    pub fn with_execution(
        &self,
        id: impl Into<String>,
        environment: impl Into<String>,
        generation: u64,
    ) -> Self {
        let mut scoped = self.clone();
        scoped.scope.execution_id = Some(id.into());
        scoped.scope.environment = Some(environment.into());
        scoped.scope.generation = Some(generation);
        scoped
    }
    pub(crate) fn with_request(&self, request: Option<String>) -> Self {
        let mut scoped = self.clone();
        scoped.scope.model_request_id = request;
        scoped
    }
    fn observe_tool(
        &self,
        context: &ToolContext,
        call_id: String,
        name: &str,
        input: &Value,
        nested: bool,
    ) -> Option<crate::observation::ToolObservation> {
        self.observation.as_ref().map(|observer| {
            let mut scope = self.scope.clone();
            scope.session_id = context.session_id.clone();
            scope.run_id = context.run_id.clone();
            scope.call_id = Some(call_id);
            scope.invocation_id = if nested || context.invocation_id.is_empty() {
                None
            } else {
                Some(context.invocation_id.clone())
            };
            let origin = if nested && self.origin == "model" {
                "host"
            } else {
                self.origin
            };
            crate::observation::ToolObservation::start(
                observer.clone(),
                scope,
                name,
                input,
                nested,
                origin,
            )
        })
    }
    pub fn definitions(&self) -> Vec<crate::ToolDefinition> {
        self.tools.capabilities()
    }
    /// Original server identity for host discovery, independent of wire names.
    pub fn tool_server(&self, name: &str) -> Option<&str> {
        self.tools.tool_server(name)
    }
    /// Removed host names may retain callable error stubs without appearing in
    /// advertised definitions or discovery results.
    pub fn unavailable_names(&self) -> Vec<String> {
        self.tools.unavailable.keys().cloned().collect()
    }
    /// Whether a host capability's full signature is always shown.
    pub fn is_shown_always(&self, name: &str) -> bool {
        self.tools.is_shown_always(name)
    }
    pub fn emit_output(
        &self,
        context: &ToolContext,
        execution_id: &str,
        output: Value,
    ) -> Result<String> {
        ensure!(!context.cancellation.is_cancelled(), "Execution cancelled");
        let record = OutputRecord {
            id: id(),
            session_id: context.session_id.clone(),
            run_id: context.run_id.clone(),
            execution_id: execution_id.into(),
            created_at: now(),
            output,
        };
        self.log.commit(
            &self.store,
            &context.session_id,
            &context.run_id,
            None,
            |tx| {
                Store::append_output_tx(tx, &record)?;
                Ok((
                    (),
                    vec![(
                        EventKind::Output {
                            record: record.clone(),
                        },
                        Some(crate::event_log::ContentRef::Output {
                            output_id: record.id.clone(),
                        }),
                    )],
                ))
            },
        )?;
        if let Some(observer) = &self.observation {
            let mut scope = self.scope.clone();
            scope.session_id = context.session_id.clone();
            scope.run_id = context.run_id.clone();
            scope.execution_id = Some(execution_id.into());
            let bytes = record.output.to_string();
            observer.emit(scope, "output_emitted", serde_json::json!({"output_type":record.output.get("type").and_then(Value::as_str),"status":record.output.get("status").and_then(Value::as_str),"output_bytes":bytes.len(),"fingerprint":crate::fingerprint(bytes.as_bytes())}), None);
        }
        let output_id = record.id.clone();
        Ok(output_id)
    }
    pub fn complete_effect(&self, invocation: &str) -> Result<()> {
        self.store.effect_completed(invocation)
    }
    fn limit_model_result(&self, context: &ToolContext, result: ToolResult) -> Result<ToolResult> {
        let result = if let Some(limit) = &self.tools.result_limiter {
            limit(&self.store, context, result, self.max_result_bytes)?
        } else {
            result
        };
        if result.content.len() > self.max_result_bytes
            && let Ok(value) = serde_json::from_str::<Value>(&result.content)
        {
            let mut summary = serde_json::json!({"error":"Tool result exceeds size limit; request smaller output. Work already performed is not undone.","resultTruncated":true});
            for key in ["handle", "status", "cursor", "exitCode"] {
                if let Some(v) = value.get(key) {
                    summary[key] = v.clone();
                }
            }
            return Ok(ToolResult {
                content: summary.to_string(),
                is_error: true,
                pending: result.pending,
            });
        }
        Ok(ToolResult {
            content: super::runtime::bounded(&result.content, self.max_result_bytes),
            ..result
        })
    }
}

#[cfg(test)]
mod workspace_tests {
    use super::*;
    use crate::ContextStrategy;

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
        let dispatcher = ToolDispatcher::new(store.clone(), ToolRegistry::new(), 4096, None);
        assert_eq!(dispatcher.workspace_roots(&root_a.id)?, vec![a.clone()]);
        assert_eq!(dispatcher.workspace_roots(&root_b.id)?, vec![b.clone()]);
        let child_id = child.id.clone();
        let effective = worktree.clone();
        let dispatcher = dispatcher.with_workspace_resolver(Some(Arc::new(move |session| {
            (session == child_id).then(|| effective.clone())
        })));
        let roots = dispatcher.workspace_roots(&child.id)?;
        assert_eq!(roots, vec![worktree.clone(), b.clone()]);
        assert!(!dispatcher.workspace_roots(&child.id)?.contains(&a));
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
