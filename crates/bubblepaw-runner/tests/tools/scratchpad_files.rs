use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, RunOptions,
    Store, TextCallback, ToolRegistry, ToolResult,
};
use bubblepaw_runner::tools::WorkspaceTools;
use serde_json::{Value, json};
use std::{fs, sync::Arc};

struct NoProvider;
#[async_trait]
impl Provider for NoProvider {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        panic!("file-tool tests must not invoke a model")
    }
}

struct Fixture {
    root: tempfile::TempDir,
    workspace: WorkspaceTools,
    store: Store,
    session: String,
    harness: Harness,
}

impl Fixture {
    fn new() -> Result<Self> {
        let root = tempfile::tempdir()?;
        let workspace = WorkspaceTools::new(root.path())?;
        // Deliberately inside the workspace to catch accidental workspace grants
        // to sibling scratch directories, and with an in-memory dispatcher Store.
        workspace.prepare_scratchpads(root.path().join("host-data"))?;
        let store = Store::in_memory()?;
        let session = store.create_session("scratch files")?.id;
        let mut tools = ToolRegistry::new();
        workspace.register(&mut tools, false)?;
        let harness = Harness::new(
            store.clone(),
            Arc::new(NoProvider),
            tools,
            HarnessConfig::default(),
        );
        Ok(Self {
            root,
            workspace,
            store,
            session,
            harness,
        })
    }

    async fn call(&self, name: &str, input: Value) -> Result<ToolResult> {
        self.harness
            .call_tool(
                &self.session,
                name,
                input,
                RunOptions::default(),
                CancellationToken::new(),
            )
            .await
    }
}

#[tokio::test]
async fn file_tools_read_write_and_edit_both_scratchpads_through_the_dispatcher() -> Result<()> {
    let fixture = Fixture::new()?;
    let paths = fixture
        .workspace
        .scratchpads(&fixture.store, &fixture.session)?;
    for root in [paths.chat, paths.workspace] {
        let path = root.join("nested/script.txt");
        let result = fixture
            .call("write", json!({"path":path,"content":"old\r\n"}))
            .await?;
        assert!(!result.is_error, "{}", result.content);
        let value: Value = serde_json::from_str(&result.content)?;
        assert_eq!(value, json!({"path":path,"bytes":5,"created":true}));
        let result = fixture
            .call(
                "edit",
                json!({"path":path,"old_string":"old","new_string":"new"}),
            )
            .await?;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            serde_json::from_str::<Value>(&result.content)?,
            json!({"path":path,"replacements":1})
        );
        let result = fixture.call("read", json!({"path":path})).await?;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            serde_json::from_str::<Value>(&result.content)?["content"],
            "new\r\n"
        );
        assert_eq!(fs::read(&path)?, b"new\r\n");
    }
    assert!(
        fixture
            .store
            .checkpoint_records_page(&fixture.session, None, 100)?
            .is_empty(),
        "disposable scratch files are not workspace rewind checkpoints"
    );
    let result = fixture
        .call(
            "write",
            json!({"path":"workspace.txt","content":"workspace"}),
        )
        .await?;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("workspace.txt"))?,
        "workspace"
    );
    assert_eq!(
        fixture
            .store
            .checkpoint_records_page(&fixture.session, None, 100)?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn sibling_scratch_and_parent_traversal_do_not_fall_back_to_workspace_access() -> Result<()> {
    let fixture = Fixture::new()?;
    let other = fixture.store.create_session("another chat")?;
    let other_paths = fixture.workspace.scratchpads(&fixture.store, &other.id)?;
    fs::write(other_paths.chat.join("private.txt"), "private")?;
    let paths = fixture
        .workspace
        .scratchpads(&fixture.store, &fixture.session)?;
    for path in [
        other_paths.chat.join("private.txt"),
        paths.chat.join("../outside.txt"),
    ] {
        for (tool, input) in [
            ("read", json!({"path":path})),
            ("write", json!({"path":path,"content":"overwrite"})),
            (
                "edit",
                json!({"path":path,"old_string":"private","new_string":"overwrite"}),
            ),
        ] {
            let result = fixture.call(tool, input).await?;
            assert!(
                result.is_error,
                "{tool} unexpectedly accepted {}",
                path.display()
            );
        }
    }
    assert_eq!(
        fs::read_to_string(other_paths.chat.join("private.txt"))?,
        "private"
    );
    Ok(())
}

async fn reject_tool_link_escape(link: impl Fn(&std::path::Path, &std::path::Path)) -> Result<()> {
    let fixture = Fixture::new()?;
    let paths = fixture
        .workspace
        .scratchpads(&fixture.store, &fixture.session)?;
    fs::write(fixture.root.path().join("target.txt"), "original")?;
    for root in [&paths.chat, &paths.workspace] {
        link(&root.join("escape"), fixture.root.path());
        let path = root.join("escape/target.txt");
        for (tool, input) in [
            ("read", json!({"path":path})),
            ("write", json!({"path":path,"content":"overwrite"})),
            (
                "edit",
                json!({"path":path,"old_string":"original","new_string":"overwrite"}),
            ),
        ] {
            let result = fixture.call(tool, input).await?;
            assert!(
                result.is_error,
                "{tool} unexpectedly followed scratch escape"
            );
        }
        let result = fixture
            .call(
                "write",
                json!({"path":root.join("escape/new/file"),"content":"outside"}),
            )
            .await?;
        assert!(result.is_error);
    }
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("target.txt"))?,
        "original"
    );
    assert!(!fixture.root.path().join("new").exists());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn file_tools_refuse_scratch_symlink_escape_into_workspace() -> Result<()> {
    reject_tool_link_escape(|link, target| std::os::unix::fs::symlink(target, link).unwrap()).await
}

#[cfg(windows)]
#[tokio::test]
async fn file_tools_refuse_scratch_junction_escape_into_workspace() -> Result<()> {
    reject_tool_link_escape(|link, target| {
        let plain = |path: &std::path::Path| {
            path.to_string_lossy()
                .trim_start_matches(r"\\?\")
                .to_owned()
        };
        let output = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(plain(link))
            .arg(plain(target))
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

#[cfg(windows)]
#[tokio::test]
async fn file_tools_accept_ordinary_windows_absolute_scratch_paths() -> Result<()> {
    let fixture = Fixture::new()?;
    let paths = fixture
        .workspace
        .scratchpads(&fixture.store, &fixture.session)?;
    for root in [paths.chat, paths.workspace] {
        let target = root.join("ordinary.txt");
        let path = target
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_owned();
        let result = fixture
            .call("write", json!({"path":path,"content":"before"}))
            .await?;
        assert!(!result.is_error, "{}", result.content);
        let result = fixture
            .call(
                "edit",
                json!({"path":path,"old_string":"before","new_string":"after"}),
            )
            .await?;
        assert!(!result.is_error, "{}", result.content);
        let result = fixture.call("read", json!({"path":path})).await?;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            serde_json::from_str::<Value>(&result.content)?["content"],
            "after"
        );
        assert_eq!(fs::read_to_string(target)?, "after");
    }
    Ok(())
}
