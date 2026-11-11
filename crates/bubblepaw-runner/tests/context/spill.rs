//! Scripted model and real local transport only; no live model calls.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, Role,
    RunOptions, Store, TextCallback, Tool, ToolCall, ToolContext, ToolDefinition, ToolRegistry,
    ToolResult,
};
use bubblepaw_runner::{
    artifacts::ArtifactStore, codemode::CodeMode, config::ExecConfig,
    register_native_tools_with_exec, spill::SpillStore, tools::ProcessTools,
};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};

struct Cells;
#[async_trait]
impl Provider for Cells {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        if let Some(message) = request
            .messages
            .last()
            .filter(|message| message.role == Role::User)
            && let Ok(input) = serde_json::from_str::<Value>(&message.content)
        {
            return Ok(ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "js".into(),
                    arguments: input,
                }],
                ..Default::default()
            });
        }
        Ok(ModelResponse {
            text: "complete".into(),
            ..Default::default()
        })
    }
}
struct Large(Arc<String>);
#[async_trait]
impl Tool for Large {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "large".into(),
            description: "Fixture large native result".into(),
            input_schema: json!({"type":"object"}),
            read_only: true,
            approval: Default::default(),
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        Ok(ToolResult::text(self.0.as_str()))
    }
}
struct Fixture {
    workspace: tempfile::TempDir,
    data: tempfile::TempDir,
    store: Store,
    session: String,
    harness: Harness,
    mode: Arc<CodeMode>,
    processes: Arc<ProcessTools>,
}
impl Fixture {
    fn new(large: String) -> Self {
        Self::with_exec(large, ExecConfig::default())
    }
    fn with_exec(large: String, exec: ExecConfig) -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let store = Store::open_data_dir(data.path()).unwrap();
        let session = store.create_session("spill").unwrap().id;
        let mut registry = ToolRegistry::new();
        let processes = register_native_tools_with_exec(
            &mut registry,
            workspace.path(),
            false,
            exec,
            data.path(),
        )
        .unwrap();
        registry
            .register_host(Arc::new(Large(Arc::new(large))))
            .unwrap();
        let artifacts =
            Arc::new(ArtifactStore::new(workspace.path(), data.path().join("artifacts")).unwrap());
        let mode = Arc::new(CodeMode::new(workspace.path().to_owned(), artifacts).unwrap());
        mode.register(&mut registry).unwrap();
        let harness = Harness::new(
            store.clone(),
            Arc::new(Cells),
            registry,
            HarnessConfig {
                execution_mode: bubblepaw_core::ExecutionMode::Js,
                ..Default::default()
            },
        );
        Self {
            workspace,
            data,
            store,
            session,
            harness,
            mode,
            processes,
        }
    }
    async fn cell_for(&self, session: &str, code: &str) -> ToolResult {
        self.harness
            .prompt(
                session,
                json!({"code":code}).to_string(),
                RunOptions::default(),
            )
            .await
            .unwrap();
        let message = self
            .store
            .messages(session)
            .unwrap()
            .into_iter()
            .rev()
            .find(|message| message.role == Role::Tool)
            .unwrap();
        ToolResult {
            content: message.content,
            is_error: message.is_error,
            pending: false,
        }
    }
    async fn cell(&self, code: &str) -> Value {
        let result = self.cell_for(&self.session, code).await;
        assert!(!result.is_error, "{}", result.content);
        serde_json::from_str(result.content.split("\n[---]\n").next().unwrap()).unwrap()
    }
    async fn shutdown(&self) {
        self.mode.shutdown().await;
        self.processes.shutdown().await;
    }
}

#[tokio::test]
async fn js_returned_native_text_spills_all_bytes_and_reads_own_files_only() {
    let full = (0..10_000)
        .map(|line| format!("line-{line:05} full native payload\n"))
        .collect::<String>();
    let fixture = Fixture::new(full.clone());
    let result = fixture.cell("return await tools.large({});").await;
    assert_eq!(result["truncated"], true);
    assert!(result["preview"].as_str().unwrap().contains("line-00000"));
    assert!(result["preview"].as_str().unwrap().contains("line-09999"));
    let path = result["fullOutput"]["path"].as_str().unwrap();
    assert!(Path::new(path).is_absolute());
    assert!(
        Path::new(path).starts_with(
            fixture
                .data
                .path()
                .canonicalize()
                .unwrap()
                .join("spill")
                .join(&fixture.session)
        )
    );
    let saved = std::fs::read_to_string(path).unwrap();
    assert_eq!(serde_json::from_str::<String>(&saved).unwrap(), full);
    assert_eq!(result["fullOutput"]["bytes"], saved.len());
    let read = fixture
        .cell(&format!(
            "return await tools.read({{path:{},startLine:1,lineCount:1}});",
            json!(path)
        ))
        .await;
    assert_eq!(read["truncated"], true);
    assert!(saved.starts_with(read["content"].as_str().unwrap()));
    let other = fixture.store.create_session("other").unwrap().id;
    let rejected = fixture
        .cell_for(
            &other,
            &format!("return await tools.read({{path:{}}});", json!(path)),
        )
        .await;
    assert!(rejected.is_error);
    assert!(
        rejected.content.contains("outside this chat"),
        "{}",
        rejected.content
    );
    let traversal = Path::new(path)
        .parent()
        .unwrap()
        .join("..")
        .join(&fixture.session)
        .join(Path::new(path).file_name().unwrap());
    let rejected = fixture
        .cell_for(
            &other,
            &format!("return await tools.read({{path:{}}});", json!(traversal)),
        )
        .await;
    assert!(rejected.is_error);
    let forbidden = fixture
        .cell_for(
            &fixture.session,
            &format!(
                "return await tools.list({{path:{}}});",
                json!(Path::new(path).parent().unwrap())
            ),
        )
        .await;
    assert!(forbidden.is_error);
    fixture.shutdown().await;
}

#[tokio::test]
async fn js_results_above_old_resource_ceiling_spill_without_resetting_state() {
    let fixture = Fixture::new(String::new());
    let result = fixture
        .cell("state.keep=42; return {body:'x'.repeat(1200000),tail:'complete'};")
        .await;
    let path = result["fullOutput"]["path"].as_str().unwrap();
    let full = std::fs::read_to_string(path).unwrap();
    let value: Value = serde_json::from_str(full.split("\n[---]\n").next().unwrap()).unwrap();
    assert_eq!(value["body"].as_str().unwrap().len(), 1_200_000);
    assert_eq!(value["tail"], "complete");
    assert_eq!(fixture.cell("return state.keep;").await, 42);
    assert!(!fixture.workspace.path().join(".run/js-output").exists());
    fixture.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn large_exec_spills_complete_spool_before_reaping_and_read_can_reach_tail() {
    let fixture = Fixture::new(String::new());
    let command = "awk 'BEGIN { for(i=0;i<20000;i++) printf \"line-%05d payload\\n\",i }'";
    let result = fixture
        .cell(&format!(
            "return await tools.exec({{command:{}}});",
            json!(command)
        ))
        .await;
    assert_eq!(result["status"], "exited");
    assert!(
        result["diagnostic"]
            .as_str()
            .unwrap()
            .contains("complete decoded output")
    );
    assert!(
        !result["diagnostic"]
            .as_str()
            .unwrap()
            .contains("Capture is incomplete")
    );
    let path = result["fullOutput"]["path"].as_str().unwrap();
    let full = std::fs::read_to_string(path).unwrap();
    let stdout = full
        .split_once("[stdout]\n")
        .unwrap()
        .1
        .split_once("\n[stderr]\n")
        .unwrap()
        .0;
    let expected = (0..20_000)
        .map(|line| format!("line-{line:05} payload\n"))
        .collect::<String>();
    assert_eq!(stdout, expected);
    assert_eq!(result["fullOutput"]["bytes"], full.len());
    let read = fixture
        .cell(&format!(
            "return await tools.read({{path:{},startLine:20002,lineCount:3}});",
            json!(path)
        ))
        .await;
    assert!(
        read["content"]
            .as_str()
            .unwrap()
            .contains("line-19999 payload")
    );
    let second = fixture
        .cell(&format!(
            "return await tools.poll({{handle:{}}});",
            result["handle"]
        ))
        .await;
    assert_eq!(second["fullOutput"], result["fullOutput"]);
    assert_eq!(std::fs::read_to_string(path).unwrap(), full);
    fixture.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn output_beyond_exec_capture_cap_never_claims_a_complete_spill() {
    let fixture = Fixture::with_exec(
        String::new(),
        ExecConfig {
            max_output_bytes: 4096,
            ..Default::default()
        },
    );
    let result = fixture.cell("return await tools.exec({command:\"awk 'BEGIN {for(i=0;i<10000;i++)print \\\"payload line\\\"}'\"});").await;
    assert_eq!(result["status"], "exited");
    assert!(result.get("fullOutput").is_none());
    assert!(
        result["fullOutputError"]
            .as_str()
            .unwrap()
            .contains("capture limit")
    );
    assert!(
        result["diagnostic"]
            .as_str()
            .unwrap()
            .contains("Capture is incomplete")
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn spill_line_ranges_allow_files_over_workspace_limit_with_bounded_reply() {
    let fixture = Fixture::new(String::new());
    let spills = SpillStore::new(fixture.data.path()).unwrap();
    let body = "payload line\n".repeat(750_000);
    let reference = spills.save(&fixture.session, &body).unwrap();
    assert!(body.len() > 8 * 1024 * 1024);
    let read = fixture
        .cell(&format!(
            "return await tools.read({{path:{},startLine:749999,lineCount:2}});",
            reference["path"]
        ))
        .await;
    assert_eq!(read["content"], "payload line\npayload line\n");
    assert_eq!(read["totalLines"], 750_000);
    assert_eq!(read["truncated"], false);
    fixture.shutdown().await;
}

#[test]
fn per_chat_retention_evicts_oldest_without_sqlite_writes_or_cross_chat_eviction() {
    let data = tempfile::tempdir().unwrap();
    let store = Store::open_data_dir(data.path()).unwrap();
    let session = store.create_session("retention").unwrap();
    let other = store.create_session("retained").unwrap();
    let spills = SpillStore::with_chat_limit(data.path(), 150_000).unwrap();
    let before = store.storage_stats().unwrap();
    let first = spills.save(&session.id, &"a".repeat(70_000)).unwrap();
    let second = spills.save(&session.id, &"b".repeat(70_000)).unwrap();
    let separate = spills.save(&other.id, &"z".repeat(140_000)).unwrap();
    let third = spills.save(&session.id, &"c".repeat(70_000)).unwrap();
    let after = store.storage_stats().unwrap();
    assert!(!Path::new(first["path"].as_str().unwrap()).exists());
    for reference in [&second, &third, &separate] {
        assert!(Path::new(reference["path"].as_str().unwrap()).exists());
    }
    assert_eq!(after.transaction_count, before.transaction_count);
    assert_eq!(after.database_bytes, before.database_bytes);
    assert_eq!(after.wal_bytes, before.wal_bytes);
    println!(
        "spill fixture: retained text bytes=280000, SQLite database {} -> {}, WAL {} -> {}, application transactions {} -> {}",
        before.database_bytes,
        after.database_bytes,
        before.wal_bytes,
        after.wal_bytes,
        before.transaction_count,
        after.transaction_count
    );
    assert!(spills.save(&session.id, &"x".repeat(150_001)).is_err());
}

#[test]
fn archive_keeps_spills_and_reopened_host_cleanup_deletes_only_removed_chat() {
    let data = tempfile::tempdir().unwrap();
    let spills = SpillStore::new(data.path()).unwrap();
    let session;
    let retained;
    let child;
    let child_output;
    let first;
    let second;
    {
        let store = Store::open_data_dir(data.path()).unwrap();
        session = store.create_session("delete").unwrap().id;
        retained = store.create_session("keep").unwrap().id;
        child = store
            .create_session_with_visibility(
                "hidden child",
                Default::default(),
                Some(&session),
                true,
            )
            .unwrap()
            .id;
        first = spills.save(&session, "one").unwrap();
        child_output = spills.save(&child, "child output").unwrap();
        second = spills.save(&retained, "two").unwrap();
        assert!(store.archive_chat(&session).unwrap());
        assert!(Path::new(first["path"].as_str().unwrap()).exists());
        assert!(Path::new(child_output["path"].as_str().unwrap()).exists());
    }
    let reopened = Store::open_data_dir(data.path()).unwrap();
    spills.attach(&reopened);
    reopened.delete_session(&session).unwrap();
    assert!(!Path::new(first["path"].as_str().unwrap()).exists());
    assert!(!Path::new(child_output["path"].as_str().unwrap()).exists());
    assert!(reopened.session(&child).is_err());
    assert!(Path::new(second["path"].as_str().unwrap()).exists());
    assert!(reopened.session(&retained).is_ok());
}

#[cfg(unix)]
#[tokio::test]
async fn spill_symlinks_cannot_grant_other_chats_or_external_files() {
    let fixture = Fixture::new(String::new());
    let spills = SpillStore::new(fixture.data.path()).unwrap();
    let own = spills.save(&fixture.session, "own").unwrap();
    let other = fixture.store.create_session("other").unwrap().id;
    let foreign = spills.save(&other, "private").unwrap();
    let link = Path::new(own["path"].as_str().unwrap())
        .parent()
        .unwrap()
        .join("escape.txt");
    std::os::unix::fs::symlink(foreign["path"].as_str().unwrap(), &link).unwrap();
    let result = fixture
        .cell_for(
            &fixture.session,
            &format!("return await tools.read({{path:{}}});", json!(link)),
        )
        .await;
    assert!(result.is_error);
    let workspace_link = fixture.workspace.path().join("foreign.txt");
    std::os::unix::fs::symlink(foreign["path"].as_str().unwrap(), &workspace_link).unwrap();
    let result = fixture
        .cell_for(
            &fixture.session,
            "return await tools.read({path:'foreign.txt'});",
        )
        .await;
    assert!(result.is_error);
    fixture.shutdown().await;
}
