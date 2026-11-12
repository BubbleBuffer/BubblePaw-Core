use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, Role,
    RunOptions, SCRIPT_MODEL, Store, TextCallback, Tool, ToolCall, ToolContext, ToolDefinition,
    ToolRegistry, ToolResult,
};
use bubblepaw_runner::{artifacts::ArtifactStore, codemode::CodeMode};
use serde_json::{Value, json};
use std::{
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Fails the test if a script run reaches it; otherwise runs the code in the
/// last user message once, then finishes.
#[derive(Default)]
struct Model {
    requests: Mutex<Vec<ModelRequest>>,
}
#[async_trait]
impl Provider for Model {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        assert_ne!(
            request.model, SCRIPT_MODEL,
            "a script run called the provider"
        );
        let last = request.messages.last().cloned();
        self.requests.lock().unwrap().push(request);
        if let Some(last) = last.filter(|m| m.role == Role::User) {
            return Ok(ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "js".into(),
                    arguments: json!({"code": last.content}),
                }],
                ..Default::default()
            });
        }
        Ok(ModelResponse {
            text: "model finished".into(),
            ..Default::default()
        })
    }
}

struct Host(&'static str, Arc<tokio::sync::Notify>);
#[async_trait]
impl Tool for Host {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.0.into(),
            description: "Test capability".into(),
            read_only: true,
            approval: Default::default(),
            input_schema: json!({"type":"object"}),
        }
    }
    async fn invoke(&self, _: ToolContext, input: Value) -> Result<ToolResult> {
        match self.0 {
            "echo" => Ok(ToolResult::text(
                json!({"echo": input["value"]}).to_string(),
            )),
            _ => {
                self.1.notify_one();
                std::future::pending().await
            }
        }
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    store: Store,
    harness: Harness,
    model: Arc<Model>,
    session: String,
    stalled: Arc<tokio::sync::Notify>,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let artifacts =
            Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap());
        let repl = Arc::new(CodeMode::new(root.path().to_owned(), artifacts).unwrap());
        let stalled = Arc::new(tokio::sync::Notify::new());
        let mut tools = ToolRegistry::new();
        for name in ["echo", "stall"] {
            tools
                .register_host(Arc::new(Host(name, stalled.clone())))
                .unwrap();
        }
        repl.register(&mut tools).unwrap();
        let store = Store::in_memory().unwrap();
        let session = store.create_session("scripts").unwrap().id;
        let model = Arc::new(Model::default());
        let harness = Harness::new(
            store.clone(),
            model.clone(),
            tools,
            HarnessConfig {
                execution_mode: bubblepaw_core::ExecutionMode::Js,
                ..Default::default()
            },
        );
        Self {
            _root: root,
            store,
            harness,
            model,
            session,
            stalled,
        }
    }
    fn script(code: &str) -> RunOptions {
        RunOptions {
            script: Some(code.into()),
            ..Default::default()
        }
    }
}

#[tokio::test]
async fn script_run_calls_a_host_tool_without_a_provider() {
    let f = Fixture::new();
    let run = f
        .harness
        .prompt(
            &f.session,
            "[automation] Echo",
            Fixture::script("return await tools.echo({value:'hi'});"),
        )
        .await
        .unwrap();
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(run.model, SCRIPT_MODEL);
    assert_eq!(
        serde_json::from_str::<Value>(run.text.as_deref().unwrap()).unwrap(),
        json!({"echo":"hi"})
    );
    assert!(f.model.requests.lock().unwrap().is_empty());
    let messages = f.store.messages(&f.session).unwrap();
    assert_eq!(
        messages.iter().map(|m| m.role.clone()).collect::<Vec<_>>(),
        [Role::User, Role::Assistant, Role::Tool]
    );
    assert_eq!(messages[0].content, "[automation] Echo");
    assert_eq!(messages[1].content, "");
    assert_eq!(messages[1].provider_state, Some(json!({"script":true})));
    let call = &messages[1].tool_calls[0];
    assert_eq!(call.name, "js");
    assert_eq!(
        call.arguments,
        json!({"code":"return await tools.echo({value:'hi'});"})
    );
    assert_eq!(messages[2].tool_call_id.as_ref(), Some(&call.id));
    assert!(!messages[2].is_error);
    assert!(f.store.pending_effects(&f.session).unwrap().is_empty());
}

#[tokio::test]
async fn throwing_script_fails_the_run() {
    let f = Fixture::new();
    let run = f
        .harness
        .prompt(
            &f.session,
            "[automation] Throw",
            Fixture::script("throw Error('script broke');"),
        )
        .await
        .unwrap();
    assert_eq!(run.status, "failed");
    assert!(
        run.error.as_deref().unwrap().contains("script broke"),
        "{:?}",
        run.error
    );
    let messages = f.store.messages(&f.session).unwrap();
    assert!(messages.last().unwrap().is_error);
    assert!(f.model.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn script_and_later_model_run_share_state_and_history() {
    let f = Fixture::new();
    let run = f
        .harness
        .prompt(
            &f.session,
            "[automation] Seed",
            Fixture::script("state.seed = 41; return 'seeded';"),
        )
        .await
        .unwrap();
    assert_eq!(run.status, "completed", "{:?}", run.error);
    let run = f
        .harness
        .prompt(&f.session, "return state.seed + 1;", RunOptions::default())
        .await
        .unwrap();
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(run.text.as_deref(), Some("model finished"));
    let messages = f.store.messages(&f.session).unwrap();
    let last_tool = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .unwrap();
    assert!(last_tool.content.starts_with("42"), "{}", last_tool.content);
    let requests = f.model.requests.lock().unwrap();
    let first = &requests[0].messages;
    assert!(first.iter().any(|m| {
        m.tool_calls
            .iter()
            .any(|c| c.arguments["code"] == "state.seed = 41; return 'seeded';")
    }));
    assert!(
        first
            .iter()
            .any(|m| m.role == Role::Tool && m.content.starts_with("\"seeded\""))
    );
}

#[tokio::test]
async fn cancel_stops_a_long_script() {
    let f = Fixture::new();
    f.harness
        .start(
            &f.session,
            "[automation] Stall",
            Fixture::script("await tools.stall({}); return 'never';"),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), f.stalled.notified())
        .await
        .expect("the stalled tool call starts");
    assert!(f.harness.cancel(&f.session));
    let run = tokio::time::timeout(Duration::from_secs(10), f.harness.wait_for_idle(&f.session))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.status, "cancelled", "{:?}", run.error);
    assert!(f.model.requests.lock().unwrap().is_empty());
    let pending = f.store.pending_effects(&f.session).unwrap();
    // The interrupted host call's outcome is unknown, exactly as in a model run.
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0]["state"], "unknown");
}

fn cli(root: &std::path::Path) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_bubblepaw"));
    command.args(["--set", "agent.execution_mode='js'"]);
    command
        .args(["--provider", "demo", "--no-auto-summarize", "--workspace"])
        .arg(root)
        .arg("--db")
        .arg(root.join("archive.sqlite"));
    command
}

#[test]
fn cli_script_prints_the_result() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("note.txt"), "from the workspace").unwrap();
    let file = temp.path().join("digest.js");
    std::fs::write(
        &file,
        "const r = await tools.read({path:'note.txt'}); return {read: r.content.includes('from the workspace')};",
    )
    .unwrap();
    let output = cli(temp.path()).arg("script").arg(&file).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("{\"read\":true}"), "{stdout}");
    let failing = temp.path().join("fail.js");
    std::fs::write(&failing, "throw Error('cli script broke');").unwrap();
    let output = cli(temp.path())
        .arg("script")
        .arg(&failing)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cli script broke"));
}

#[tokio::test]
async fn stdio_run_start_accepts_a_script() {
    let temp = tempfile::tempdir().unwrap();
    let mut child = tokio::process::Command::from(cli(temp.path()))
        .arg("stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut lines = Vec::new();
    let mut send = async |id: i32, method: &str, params: Value| {
        let mut bytes =
            serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
                .unwrap();
        bytes.push(b'\n');
        stdin.write_all(&bytes).await.unwrap();
        stdin.flush().await.unwrap();
    };
    async fn next(stdout: &mut BufReader<tokio::process::ChildStdout>) -> Value {
        let mut line = String::new();
        let count = tokio::time::timeout(Duration::from_secs(10), stdout.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert!(count > 0, "runner closed stdout");
        serde_json::from_str(&line).unwrap()
    }
    send(1, "session.open", json!({"title":"script"})).await;
    let session = loop {
        let record = next(&mut stdout).await;
        if record["id"] == 1 {
            break record["result"]["id"].as_str().unwrap().to_owned();
        }
    };
    send(
        2,
        "run.start",
        json!({"session_id":session,"text":"[automation] Seven","script":"return 3 + 4;"}),
    )
    .await;
    loop {
        let record = next(&mut stdout).await;
        if record["id"] == 2 {
            assert!(record.get("error").is_none(), "{record}");
            assert_eq!(record["result"]["model"], SCRIPT_MODEL);
        }
        let settled = record["params"]["type"] == "session_settled";
        lines.push(record);
        if settled {
            break;
        }
    }
    let finished = lines
        .iter()
        .find(|r| r["params"]["type"] == "run_finished")
        .unwrap();
    assert_eq!(finished["params"]["status"], "completed", "{finished}");
    assert!(
        finished["params"]["text"]
            .as_str()
            .unwrap()
            .starts_with('7'),
        "{finished}"
    );
    send(3, "shutdown", json!({})).await;
    drop(stdin);
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
}
