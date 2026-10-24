//! Workspace-root deletion checks use an inert exec tool; no shell is started.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, Role,
    RunOptions, Store, TextCallback, Tool, ToolCall, ToolContext, ToolDefinition, ToolRegistry,
    ToolResult,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct RequestExec(Value);
#[async_trait]
impl Provider for RequestExec {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(ModelResponse {
            text: "done".into(),
            tool_calls: if request
                .messages
                .last()
                .is_some_and(|message| message.role == Role::User)
            {
                vec![ToolCall {
                    id: "exec-once".into(),
                    name: "exec".into(),
                    arguments: self.0.clone(),
                }]
            } else {
                vec![]
            },
            ..Default::default()
        })
    }
}
#[derive(Default)]
struct InertExec(AtomicUsize);
#[async_trait]
impl Tool for InertExec {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "exec".into(),
            description: "Record admission without executing".into(),
            input_schema: json!({"type":"object","properties":{"command":{"type":"string"},"cwd":{"type":"string"}},"required":["command"]}),
            read_only: false,
            approval: Default::default(),
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::text("inert"))
    }
}

#[tokio::test]
async fn protected_workspace_is_resolved_against_exec_cwd() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    std::fs::create_dir(workspace.path().join("child"))?;
    let root = workspace.path().canonicalize()?.display().to_string();
    for (command, cwd, blocked) in [
        (format!("rm -rf \"{root}\""), ".", true),
        (format!("Remove-Item -Recurse -Force \"{root}\""), ".", true),
        ("rm -rf ..".into(), "child", true),
        ("rm -rf .".into(), "child", false),
    ] {
        let store = Store::in_memory()?;
        store.bind_workspace(workspace.path())?;
        let session = store.create_session("workspace checks")?;
        let tool = Arc::new(InertExec::default());
        let mut tools = ToolRegistry::new();
        tools.register(tool.clone())?;
        let harness = Harness::new(
            store.clone(),
            Arc::new(RequestExec(json!({"command": command, "cwd": cwd}))),
            tools,
            HarnessConfig::default(),
        );
        let run = harness
            .prompt(&session.id, "inspect command", RunOptions::default())
            .await?;
        assert_eq!(run.status, "completed");
        assert_eq!(
            tool.0.load(Ordering::SeqCst),
            usize::from(!blocked),
            "{command}, cwd={cwd}"
        );
        if blocked {
            assert!(
                store
                    .messages(&session.id)?
                    .iter()
                    .any(|message| message.is_error && message.content.contains("hardline"))
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn worktree_resolver_sets_the_exec_directory_and_the_host_workspace_stays_protected()
-> Result<()> {
    let host = tempfile::tempdir()?;
    let worktree = host.path().join("worktree");
    std::fs::create_dir_all(worktree.join("child"))?;
    let host_root = host.path().canonicalize()?;
    let worktree_root = worktree.canonicalize()?;
    for (command, cwd, blocked) in [
        (format!("rm -rf \"{}\"", worktree_root.display()), ".", true),
        (format!("rm -rf \"{}\"", host_root.display()), ".", true),
        ("rm -rf ..".to_owned(), "child", true),
        ("rm -rf .".to_owned(), "child", false),
    ] {
        let store = Store::in_memory()?;
        store.bind_workspace(host.path())?;
        let session = store.create_session("worktree checks")?;
        let tool = Arc::new(InertExec::default());
        let mut tools = ToolRegistry::new();
        tools.register(tool.clone())?;
        let resolved = worktree_root.clone();
        let config = HarnessConfig {
            workspace_resolver: Some(Arc::new(move |_| Some(resolved.clone()))),
            ..Default::default()
        };
        let harness = Harness::new(
            store.clone(),
            Arc::new(RequestExec(json!({"command": command, "cwd": cwd}))),
            tools,
            config,
        );
        let run = harness
            .prompt(&session.id, "inspect command", RunOptions::default())
            .await?;
        assert_eq!(run.status, "completed");
        assert_eq!(
            tool.0.load(Ordering::SeqCst),
            usize::from(!blocked),
            "{command}, cwd={cwd}"
        );
        if blocked {
            assert!(
                store
                    .messages(&session.id)?
                    .iter()
                    .any(|message| message.is_error && message.content.contains("hardline"))
            );
        }
    }
    Ok(())
}
