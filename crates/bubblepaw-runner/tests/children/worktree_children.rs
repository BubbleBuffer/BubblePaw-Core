//! Scripted worktree acceptance through the real runner JS/file/process adapters.
use anyhow::{Result, ensure};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, HarnessConfig, ModelRequest, ModelResponse, Provider, Role, Store,
    TextCallback, ToolCall, ToolContext, ToolRegistry,
};
use bubblepaw_runner::{
    agents::{AgentRegistry, AgentTools},
    artifacts::ArtifactStore,
    codemode::CodeMode,
    config::Agents,
    memory_files::MemoryFiles,
    profiles::child_agent_host,
    tools::{ProcessTools, register_native_tools},
    worktrees,
};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct Script(Mutex<HashSet<(String, String)>>, Mutex<Vec<ModelRequest>>);
#[async_trait]
impl Provider for Script {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let task = request
            .messages
            .iter()
            .rev()
            .filter(|message| message.role == Role::User)
            .find_map(|message| {
                message
                    .content
                    .lines()
                    .next()
                    .and_then(|line| serde_json::from_str::<Value>(line).ok())
            })
            .unwrap_or(json!({}));
        self.1.lock().unwrap().push(request.clone());
        let first = self
            .0
            .lock()
            .unwrap()
            .insert((request.session_id, task.to_string()));
        Ok(if first && task["code"].is_string() {
            ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "js".into(),
                    arguments: json!({"code":task["code"]}),
                }],
                ..Default::default()
            }
        } else {
            ModelResponse {
                text: "script finished".into(),
                ..Default::default()
            }
        })
    }
}
struct Host {
    agents: Arc<AgentTools>,
    repl: Arc<CodeMode>,
    processes: Arc<ProcessTools>,
}
impl Host {
    fn new(store: &Store, workspace: &Path, provider: Arc<Script>) -> Result<Self> {
        let mut tools = ToolRegistry::new();
        let processes = register_native_tools(&mut tools, workspace, false)?;
        let repl = Arc::new(CodeMode::new(
            workspace.to_owned(),
            Arc::new(ArtifactStore::new(
                workspace,
                store.data_dir().unwrap().join("artifacts"),
            )?),
        )?);
        let memory = Arc::new(MemoryFiles::new(
            store.clone(),
            workspace.to_owned(),
            None,
            true,
        ));
        let config = HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            system_prompt_resolver: Some(Arc::new(move |session, base| {
                memory.system_prompt(session, base)
            })),
            ..Default::default()
        };
        let agents = child_agent_host(
            store.clone(),
            provider,
            &tools,
            &repl,
            &config,
            workspace,
            true,
            None,
            Agents {
                auto_wake: false,
                ..Default::default()
            },
            Arc::new(AgentRegistry::default()),
            processes.clone(),
        )?;
        Ok(Self {
            agents,
            repl,
            processes,
        })
    }
    async fn stop(&self) -> Result<()> {
        self.agents.shutdown().await?;
        self.repl.shutdown().await;
        self.processes.shutdown().await;
        Ok(())
    }
    async fn wait(&self, owner: &str, child: &str) -> Result<()> {
        let result = self.agents.wait(owner, child, Some(5000)).await?;
        ensure!(
            result["status"] == "completed",
            "child did not complete: {result}"
        );
        let messages = self
            .agents
            .profile("workspace")
            .unwrap()
            .harness
            .store()
            .messages(child)?;
        ensure!(
            messages
                .iter()
                .all(|message| message.role != Role::Tool || !message.is_error),
            "child tool failed: {:?}",
            messages
                .iter()
                .filter(|message| message.is_error)
                .map(|message| &message.content)
                .collect::<Vec<_>>()
        );
        Ok(())
    }
}
fn context(session: &str) -> ToolContext {
    ToolContext {
        session_id: session.into(),
        run_id: String::new(),
        invocation_id: uuid::Uuid::new_v4().to_string(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    }
}
fn task(code: &str) -> String {
    json!({"code":code}).to_string()
}
fn write_file(path: &str, value: &str) -> String {
    format!(
        "await tools.write({});",
        json!({"path":path,"content":format!("{value}\n")})
    )
}
fn git(path: &Path, arguments: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(arguments)
        .output()?;
    ensure!(
        output.status.success(),
        "git {}: {}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
fn initialize(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    git(path, &["init", "-q"])?;
    std::fs::write(path.join("base.txt"), "committed parent evidence\n")?;
    std::fs::write(path.join("AGENTS.md"), "COMMITTED-ISOLATED-INSTRUCTIONS\n")?;
    git(path, &["add", "base.txt", "AGENTS.md"])?;
    git(
        path,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "initial",
        ],
    )?;
    Ok(())
}

#[cfg(windows)]
const WRITE_EXEC: &str = "<nul set /p=owned child process>exec.txt& cmd /c exit 0";
#[cfg(not(windows))]
const WRITE_EXEC: &str = "printf 'owned child process' > exec.txt";
#[cfg(windows)]
const WRITE_RESTORED_EXEC: &str = "<nul set /p=restored command>restored-exec.txt& cmd /c exit 0";
#[cfg(not(windows))]
const WRITE_RESTORED_EXEC: &str = "printf 'restored command' > restored-exec.txt";

#[tokio::test]
async fn isolated_js_tools_nested_heads_inherited_workspace_and_restart_continuation() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let repo = directory.path().join("repo");
    initialize(&repo)?;
    let original_head = git(&repo, &["rev-parse", "HEAD"])?;
    std::fs::write(repo.join("parent-dirty.txt"), "uncommitted parent data")?;
    std::fs::write(repo.join("AGENTS.md"), "DIRTY-PARENT-INSTRUCTIONS\n")?;
    let store = Store::open_data_dir(directory.path().join("data"))?;
    let owner = store.create_session("parent")?.id;
    let script = Arc::new(Script::default());
    let host = Host::new(&store, &repo, script.clone())?;
    let initial = format!(
        "const evidence=await tools.read({{path:'base.txt'}}); if(!JSON.stringify(evidence).includes('committed parent evidence')) throw Error('wrong workspace read'); {} const changed=await tools.edit({{path:'isolated.txt',old_string:'child write',new_string:'child edited'}}); if(changed.replacements!==1) throw Error('child edit failed'); const command=await tools.exec({{command:{}}}); return {{evidence,changed,command}};",
        write_file("isolated.txt", "child write"),
        json!(WRITE_EXEC)
    );
    let before = store.storage_stats()?;
    let child = host.agents.start(
        &context(&owner),
        json!({"task":task(&initial),"profile":"workspace","worktree":true}),
    )?;
    let child_id = child["sessionId"].as_str().unwrap();
    host.wait(&owner, child_id).await?;
    let after = store.storage_stats()?;
    println!(
        "worktree child full-turn overhead: database_bytes={} wal_bytes={} transaction_count={}",
        after.database_bytes.saturating_sub(before.database_bytes),
        after.wal_bytes.saturating_sub(before.wal_bytes),
        after
            .transaction_count
            .saturating_sub(before.transaction_count)
    );
    let info = worktrees::get(&store, child_id)?.unwrap();
    assert_eq!(child["workspace"], info.path.to_str().unwrap());
    assert_eq!(child["branch"], info.branch);
    assert!(child["note"].as_str().unwrap().contains("uncommitted"));
    assert_eq!(git(&info.path, &["rev-parse", "HEAD"])?, original_head);
    assert_eq!(
        std::fs::read_to_string(info.path.join("isolated.txt"))?,
        "child edited\n"
    );
    assert_eq!(
        std::fs::read_to_string(info.path.join("exec.txt"))?,
        "owned child process"
    );
    assert!(!info.path.join("parent-dirty.txt").exists());
    assert!(!repo.join("isolated.txt").exists());
    assert!(!repo.join("exec.txt").exists());
    {
        let requests = script.1.lock().unwrap();
        let request = requests
            .iter()
            .find(|request| request.session_id == child_id)
            .unwrap();
        assert!(
            request
                .system_prompt
                .contains("COMMITTED-ISOLATED-INSTRUCTIONS")
        );
        assert!(!request.system_prompt.contains("DIRTY-PARENT-INSTRUCTIONS"));
    }
    git(&info.path, &["add", "isolated.txt", "exec.txt"])?;
    git(
        &info.path,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "child changes",
        ],
    )?;
    let child_head = git(&info.path, &["rev-parse", "HEAD"])?;
    assert_ne!(child_head, original_head);
    let grandchild_code = format!(
        "{} return await tools.read({{path:'isolated.txt'}});",
        write_file("grandchild.txt", "own grandchild workspace")
    );
    let inherited_code = format!(
        "{} return await tools.read({{path:'isolated.txt'}});",
        write_file("inherited.txt", "shared child workspace")
    );
    let spawn = format!(
        "const isolated=await tools.agent({}); const inherited=await tools.agent({}); return {{isolated,inherited}};",
        json!({"task":task(&grandchild_code),"profile":"workspace","worktree":true}),
        json!({"task":task(&inherited_code),"profile":"workspace","worktree":false})
    );
    host.agents.send(
        &context(&owner),
        json!({"sessionId":child_id,"text":task(&spawn)}),
    )?;
    host.wait(&owner, child_id).await?;
    let descendants = store
        .list_sessions()?
        .into_iter()
        .filter(|session| session.parent_session_id.as_deref() == Some(child_id))
        .collect::<Vec<_>>();
    assert_eq!(descendants.len(), 2);
    let mut isolated_grandchild = None;
    for session in descendants {
        host.wait(child_id, &session.id).await?;
        if let Some(grandchild) = worktrees::get(&store, &session.id)? {
            assert_eq!(git(&grandchild.path, &["rev-parse", "HEAD"])?, child_head);
            assert_eq!(grandchild.parent_workspace, info.path);
            assert_eq!(
                std::fs::read_to_string(grandchild.path.join("grandchild.txt"))?,
                "own grandchild workspace\n"
            );
            assert!(!info.path.join("grandchild.txt").exists());
            isolated_grandchild = Some((session.id, grandchild));
        } else {
            assert_eq!(worktrees::workspace(&store, &session.id, &repo)?, info.path);
            let status = host
                .agents
                .status(child_id, &json!({"sessionId":session.id}))?;
            assert_eq!(status["workspace"], info.path.to_str().unwrap());
            assert_eq!(status["branch"], info.branch);
            assert!(status["note"].as_str().unwrap().contains("shared"));
        }
    }
    assert!(info.path.join("inherited.txt").exists());
    assert!(!repo.join("inherited.txt").exists());
    let (grandchild_id, grandchild) = isolated_grandchild.unwrap();
    host.agents.send(&context(child_id), json!({"sessionId":grandchild_id,"text":task(&format!("{} return 'followed';", write_file("followup.txt", "grandchild continuation")))}))?;
    host.wait(child_id, &grandchild_id).await?;
    assert!(grandchild.path.join("followup.txt").exists());
    host.stop().await?;
    drop(host);
    let restored = Host::new(&store, &repo, script)?;
    restored.agents.send(&context(child_id), json!({"sessionId":grandchild_id,"text":task(&format!("const prior=await tools.read({{path:'followup.txt'}}); {} return prior;", write_file("restored.txt", "reattached workspace")))}))?;
    restored.wait(child_id, &grandchild_id).await?;
    assert!(grandchild.path.join("restored.txt").exists());
    assert!(!repo.join("restored.txt").exists());
    assert!(!info.path.join("restored.txt").exists());
    assert_eq!(store.runs(&grandchild_id)?.len(), 3);
    restored.stop().await?;
    Ok(())
}

#[tokio::test]
async fn non_git_isolation_fails_before_child_execution() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let store = Store::open_data_dir(directory.path().join("data"))?;
    let owner = store.create_session("parent")?.id;
    let script = Arc::new(Script::default());
    let host = Host::new(&store, &workspace, script.clone())?;
    let error = host
        .agents
        .start(
            &context(&owner),
            json!({"task":task("return 'unreachable';"),"profile":"workspace","worktree":true}),
        )
        .unwrap_err();
    assert!(error.to_string().contains("git"), "{error:#}");
    assert!(script.0.lock().unwrap().is_empty());
    assert!(store.runs(&owner)?.is_empty());
    host.stop().await?;
    Ok(())
}

#[test]
fn archived_dirty_grandchild_keeps_its_clean_parent_worktree_and_branch() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let repo = directory.path().join("repo");
    initialize(&repo)?;
    let store = Store::open_data_dir(directory.path().join("data"))?;
    let root = store.create_session("parent")?;
    let child = store.create_session_with_parent("child", Default::default(), Some(&root.id))?;
    let before_binding = store.storage_stats()?;
    let parent_worktree = worktrees::create(&store, &repo, &child.id)?;
    let after_binding = store.storage_stats()?;
    println!(
        "worktree binding-only overhead: database_bytes={} wal_bytes={} transaction_count={}",
        after_binding
            .database_bytes
            .saturating_sub(before_binding.database_bytes),
        after_binding
            .wal_bytes
            .saturating_sub(before_binding.wal_bytes),
        after_binding
            .transaction_count
            .saturating_sub(before_binding.transaction_count)
    );
    let grandchild =
        store.create_session_with_parent("grandchild", Default::default(), Some(&child.id))?;
    let grandchild_worktree = worktrees::create(&store, &parent_worktree.path, &grandchild.id)?;
    std::fs::write(
        grandchild_worktree.path.join("unfinished.txt"),
        "retained grandchild work",
    )?;
    let (archived, reports) = worktrees::archive_chat(&store, &root.id)?;
    assert!(archived);
    assert_eq!(reports.len(), 2);
    assert!(
        reports
            .iter()
            .all(|report| report.status == worktrees::CleanupStatus::Retained),
        "{reports:?}"
    );
    assert!(grandchild_worktree.path.join("unfinished.txt").exists());
    assert!(parent_worktree.path.join(".git").exists());
    assert_eq!(
        git(
            &repo,
            &[
                "rev-parse",
                &format!("refs/heads/{}", parent_worktree.branch)
            ]
        )?,
        git(&repo, &["rev-parse", "HEAD"])?
    );
    assert!(store.active_session_metadata(&root.id)?.is_none());
    assert!(store.active_session_metadata(&child.id)?.is_none());
    assert!(store.active_session_metadata(&grandchild.id)?.is_none());
    Ok(())
}

#[tokio::test]
async fn explicit_send_recreates_clean_archived_worktree_with_cached_runtime() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let repo = directory.path().join("repo");
    initialize(&repo)?;
    let store = Store::open_data_dir(directory.path().join("data"))?;
    let owner = store.create_session("parent")?.id;
    let host = Host::new(&store, &repo, Arc::new(Script::default()))?;
    let child = host.agents.start(&context(&owner), json!({"task":task("return await tools.read({path:'base.txt'});"),"profile":"workspace","worktree":true}))?;
    let child_id = child["sessionId"].as_str().unwrap();
    host.wait(&owner, child_id).await?;
    let info = worktrees::get(&store, child_id)?.unwrap();
    let (archived, reports) = worktrees::archive_chat(&store, &owner)?;
    assert!(archived);
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].status, worktrees::CleanupStatus::Removed);
    assert!(!info.path.exists());
    assert!(store.active_session_metadata(child_id)?.is_none());
    let inspector = Host::new(&store, &repo, Arc::new(Script::default()))?;
    let status = inspector
        .agents
        .status(&owner, &json!({"sessionId":child_id}))?;
    assert_eq!(status["workspace"], info.path.to_str().unwrap());
    assert_eq!(status["worktreeAvailable"], false);
    assert!(
        status["handle"].is_null(),
        "unavailable adapters must not be retained"
    );
    inspector.stop().await?;
    let resumed = format!(
        "const prior=await tools.read({{path:'base.txt'}}); {} await tools.exec({{command:{}}}); return prior;",
        write_file("restored-patch.txt", "restored patch workspace"),
        json!(WRITE_RESTORED_EXEC)
    );
    host.agents.send(
        &context(&owner),
        json!({"sessionId":child_id,"text":task(&resumed)}),
    )?;
    host.wait(&owner, child_id).await?;
    assert_eq!(
        std::fs::read_to_string(info.path.join("restored-patch.txt"))?,
        "restored patch workspace\n"
    );
    assert_eq!(
        std::fs::read_to_string(info.path.join("restored-exec.txt"))?,
        "restored command"
    );
    assert!(!repo.join("restored-patch.txt").exists());
    assert!(!repo.join("restored-exec.txt").exists());
    assert!(store.active_session_metadata(child_id)?.is_some());
    assert_eq!(store.runs(child_id)?.len(), 2);
    host.stop().await?;
    Ok(())
}

#[tokio::test]
async fn isolation_rejects_a_data_directory_inside_the_git_repository() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let repo = directory.path().join("repo");
    initialize(&repo)?;
    let store = Store::open_data_dir(repo.join("data"))?;
    let owner = store.create_session("parent")?.id;
    let script = Arc::new(Script::default());
    let host = Host::new(&store, &repo, script.clone())?;
    let error = host
        .agents
        .start(
            &context(&owner),
            json!({"task":task("return 'unreachable';"),"profile":"workspace","worktree":true}),
        )
        .unwrap_err();
    assert!(error.to_string().contains("outside"), "{error:#}");
    assert!(script.0.lock().unwrap().is_empty());
    assert!(git(&repo, &["branch", "--list", "bp/*"])?.is_empty());
    host.stop().await?;
    Ok(())
}

#[path = "worktree_children/archived_workspace.rs"]
mod archived_workspace;
