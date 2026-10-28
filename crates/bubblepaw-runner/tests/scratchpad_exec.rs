use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, RunOptions,
    Store, TextCallback, ToolRegistry, ToolResult,
};
use bubblepaw_runner::{
    config::ExecConfig,
    register_native_tools_with_exec,
    scratchpads::Scratchpads,
    tools::{ProcessTools, WorkspaceTools},
};
use serde_json::{Value, json};
use std::{fs, path::Path, sync::Arc};

struct NoProvider;
#[async_trait]
impl Provider for NoProvider {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        panic!("scratch execution acceptance must not invoke a model")
    }
}

struct Fixture {
    _data: tempfile::TempDir,
    root: tempfile::TempDir,
    paths: Scratchpads,
    session: String,
    harness: Harness,
    processes: Arc<ProcessTools>,
}

impl Fixture {
    fn new() -> Result<Self> {
        let data = tempfile::tempdir()?;
        let root = tempfile::tempdir()?;
        let store = Store::in_memory()?;
        let session = store.create_session("scratch exec")?.id;
        let mut tools = ToolRegistry::new();
        let processes = register_native_tools_with_exec(
            &mut tools,
            root.path(),
            false,
            ExecConfig::default(),
            data.path(),
        )?;
        let workspace = WorkspaceTools::new(root.path())?;
        workspace.prepare_scratchpads(data.path())?;
        let paths = workspace.scratchpads(&store, &session)?;
        let harness = Harness::new(store, Arc::new(NoProvider), tools, HarnessConfig::default());
        Ok(Self {
            _data: data,
            root,
            paths,
            session,
            harness,
            processes,
        })
    }

    async fn call(&self, tool: &str, input: Value) -> Result<ToolResult> {
        self.harness
            .call_tool(
                &self.session,
                tool,
                input,
                RunOptions::default(),
                CancellationToken::new(),
            )
            .await
    }
}

fn ordinary_path(path: &Path) -> String {
    #[cfg(windows)]
    {
        path.to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_owned()
    }
    #[cfg(not(windows))]
    {
        path.to_string_lossy().into_owned()
    }
}

fn command(script: &Path, argument: &str) -> Value {
    #[cfg(windows)]
    let prefix = "powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File ";
    #[cfg(not(windows))]
    let prefix = "sh ";
    json!({"parts":[prefix," ",""],"values":[ordinary_path(script),argument]})
}

#[cfg(windows)]
const SCRIPT_NAME: &str = "script.ps1";
#[cfg(windows)]
const SCRIPT: &str = "param([string]$value)\r\n[IO.File]::WriteAllText((Join-Path (Get-Location) 'executed.txt'), $value)\r\n[Console]::Write($value)\r\n";
#[cfg(not(windows))]
const SCRIPT_NAME: &str = "script.sh";
#[cfg(not(windows))]
const SCRIPT: &str = "printf '%s' \"$1\" > executed.txt\nprintf '%s' \"$1\"\n";

#[tokio::test]
async fn scratch_scripts_execute_with_literal_argv_in_chat_and_project_workdirs() -> Result<()> {
    let fixture = Fixture::new()?;
    for root in [&fixture.paths.chat, &fixture.paths.workspace] {
        let script = root.join(SCRIPT_NAME);
        let write = fixture
            .call("write", json!({"path":script,"content":SCRIPT}))
            .await?;
        assert!(!write.is_error, "{}", write.content);
        let read = fixture
            .call("read", json!({"path":ordinary_path(&script)}))
            .await?;
        assert!(!read.is_error, "{}", read.content);
        assert_eq!(
            serde_json::from_str::<Value>(&read.content)?["content"],
            SCRIPT
        );
        // One argument with spaces and shell syntax proves this is a literal
        // argv invocation of the file, rather than source concatenation.
        let argument = "literal value & no-shell-effect";
        let executed = fixture
            .call(
                "exec",
                json!({
                    "command":command(&script, argument),
                    "workdir":ordinary_path(root),
                    "timeoutMs":10_000
                }),
            )
            .await?;
        assert!(!executed.is_error, "{}", executed.content);
        let value: Value = serde_json::from_str(&executed.content)?;
        assert_eq!(value["status"], "exited", "{value}");
        assert_eq!(value["exitCode"], 0, "{value}");
        assert_eq!(value["stdout"], argument, "{value}");
        assert_eq!(fs::read_to_string(root.join("executed.txt"))?, argument);
        assert!(!fixture.root.path().join("executed.txt").exists());
        let equivalent = fixture
            .call(
                "exec",
                json!({
                    "command":command(&script, "both aliases"),
                    "cwd":root,
                    "workdir":ordinary_path(root),
                    "timeoutMs":10_000
                }),
            )
            .await?;
        assert!(!equivalent.is_error, "{}", equivalent.content);
        assert_eq!(
            serde_json::from_str::<Value>(&equivalent.content)?["exitCode"],
            0
        );
        assert_eq!(
            fs::read_to_string(root.join("executed.txt"))?,
            "both aliases"
        );
    }
    let conflict = fixture.call("exec", json!({"command":"echo refused","cwd":fixture.paths.chat,"workdir":fixture.paths.workspace})).await?;
    assert!(conflict.is_error);
    fixture.processes.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn exec_scratch_workdir_refuses_parent_components_before_normalization() -> Result<()> {
    let fixture = Fixture::new()?;
    for root in [&fixture.paths.chat, &fixture.paths.workspace] {
        let traversal = format!(
            "{}/../{}",
            root.display(),
            root.file_name().unwrap().to_string_lossy()
        );
        for alias in ["cwd", "workdir"] {
            let mut input = json!({"command":"echo forbidden > escaped.txt","timeoutMs":1000});
            input[alias] = json!(traversal);
            let result = fixture.call("exec", input).await?;
            assert!(
                result.is_error,
                "{alias} accepted scratch traversal: {}",
                result.content
            );
        }
        assert!(!root.join("escaped.txt").exists());
    }
    fixture.processes.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn independent_process_workspace_views_keep_the_hosts_configured_scratch_data() -> Result<()>
{
    let data = tempfile::tempdir()?;
    let root = tempfile::tempdir()?;
    let child_root = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let session = store.create_session("independent process view")?;
    let mut root_tools = ToolRegistry::new();
    let host = register_native_tools_with_exec(
        &mut root_tools,
        root.path(),
        false,
        ExecConfig::default(),
        data.path(),
    )?;
    let view = Arc::new(host.for_workspace(WorkspaceTools::new(child_root.path())?));
    let paths = bubblepaw_runner::scratchpads::ScratchpadStore::new(data.path())?.for_session(
        &store,
        &session.id,
        child_root.path(),
    )?;
    let mut tools = ToolRegistry::new();
    view.register(&mut tools)?;
    let harness = Harness::new(store, Arc::new(NoProvider), tools, HarnessConfig::default());
    let result = harness
        .call_tool(
            &session.id,
            "exec",
            json!({
                "command":"echo child-view > independent.txt",
                "workdir":ordinary_path(&paths.workspace),
                "timeoutMs":10_000
            }),
            RunOptions::default(),
            CancellationToken::new(),
        )
        .await?;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        serde_json::from_str::<Value>(&result.content)?["exitCode"],
        0
    );
    assert_eq!(
        fs::read_to_string(paths.workspace.join("independent.txt"))?.trim(),
        "child-view"
    );
    assert!(!root.path().join("independent.txt").exists());
    assert!(!child_root.path().join("independent.txt").exists());
    host.shutdown().await;
    Ok(())
}

async fn reject_workdir_link(link: impl Fn(&Path, &Path)) -> Result<()> {
    let fixture = Fixture::new()?;
    for root in [&fixture.paths.chat, &fixture.paths.workspace] {
        let escape = root.join("escape");
        link(&escape, fixture.root.path());
        let result = fixture
            .call(
                "exec",
                json!({"command":"echo forbidden > escaped.txt","workdir":escape,"timeoutMs":1000}),
            )
            .await?;
        assert!(
            result.is_error,
            "scratch workdir followed link into workspace: {}",
            result.content
        );
    }
    assert!(!fixture.root.path().join("escaped.txt").exists());
    fixture.processes.shutdown().await;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn exec_scratch_workdir_refuses_symlink_to_workspace() -> Result<()> {
    reject_workdir_link(|link, target| std::os::unix::fs::symlink(target, link).unwrap()).await
}

#[cfg(windows)]
#[tokio::test]
async fn exec_scratch_workdir_refuses_junction_to_workspace() -> Result<()> {
    reject_workdir_link(|link, target| {
        let output = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(ordinary_path(link))
            .arg(ordinary_path(target))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    })
    .await
}
