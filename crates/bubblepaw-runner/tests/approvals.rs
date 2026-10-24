//! Approvals through `js` cells and the runner surfaces (docs/approvals.md).
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    Approval, ApprovalPolicy, ApprovalRecord, CancellationToken, Decision, EventKind, Harness,
    HarnessConfig, HarnessEvent, ModelRequest, ModelResponse, Provider, Role, RunOptions, Store,
    TextCallback, Tool, ToolCall, ToolContext, ToolDefinition, ToolRegistry, ToolResult,
};
use bubblepaw_runner::{artifacts::ArtifactStore, codemode::CodeMode};
use serde_json::{Value, json};
use std::{
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout},
    sync::broadcast,
};

/// The user message is the js call's arguments.
struct Cells;
#[async_trait]
impl Provider for Cells {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        if let Some(last) = request.messages.last().filter(|m| m.role == Role::User)
            && let Ok(arguments) = serde_json::from_str::<Value>(&last.content)
        {
            return Ok(ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "js".into(),
                    arguments,
                }],
                ..Default::default()
            });
        }
        Ok(ModelResponse {
            text: "finished".into(),
            ..Default::default()
        })
    }
}
struct Effect(Arc<AtomicUsize>);
#[async_trait]
impl Tool for Effect {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "effect".into(),
            description: "An effect that needs a person's approval".into(),
            input_schema: json!({"type":"object"}),
            read_only: false,
            approval: Approval::Confirm,
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::text(json!({"done":true}).to_string()))
    }
}
struct Fixture {
    _root: tempfile::TempDir,
    store: Store,
    harness: Harness,
    _manager: Arc<CodeMode>,
    session: String,
    calls: Arc<AtomicUsize>,
}
impl Fixture {
    fn new() -> Self {
        Self::with_idle_limit(Duration::from_secs(30))
    }
    fn with_idle_limit(idle_limit: Duration) -> Self {
        let root = tempfile::tempdir().unwrap();
        let artifacts =
            Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap());
        let manager = Arc::new(
            CodeMode::new(root.path().to_owned(), artifacts)
                .unwrap()
                .with_idle_limit(idle_limit),
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let mut tools = ToolRegistry::new();
        tools
            .register_host(Arc::new(Effect(calls.clone())))
            .unwrap();
        manager.register(&mut tools).unwrap();
        let store = Store::in_memory().unwrap();
        let session = store.create_session("approvals").unwrap().id;
        let harness = Harness::new(
            store.clone(),
            Arc::new(Cells),
            tools,
            HarnessConfig {
                execution_mode: bubblepaw_core::ExecutionMode::Js,
                approval: ApprovalPolicy::default(),
                ..Default::default()
            },
        );
        Self {
            _root: root,
            store,
            harness,
            _manager: manager,
            session,
            calls,
        }
    }
    fn start(&self, code: &str) -> broadcast::Receiver<HarnessEvent> {
        let events = self.harness.subscribe();
        self.harness
            .start(
                &self.session,
                json!({ "code": code }).to_string(),
                RunOptions::default(),
            )
            .unwrap();
        events
    }
    fn result(&self) -> String {
        self.store
            .messages(&self.session)
            .unwrap()
            .into_iter()
            .rev()
            .find(|m| m.role == Role::Tool)
            .unwrap()
            .content
    }
}
async fn requested(events: &mut broadcast::Receiver<HarnessEvent>) -> ApprovalRecord {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let EventKind::ApprovalRequested { approval } = events.recv().await.unwrap().kind {
                return approval;
            }
        }
    })
    .await
    .expect("approval requested")
}
const CATCHING: &str = "try { const r = await tools.effect({}); return {ran: r.done}; } catch (e) { return {caught: e.message}; }";

#[tokio::test]
async fn js_host_call_waits_then_approval_runs_and_denial_rejects_the_await() {
    let fixture = Fixture::new();
    let mut events = fixture.start(CATCHING);
    let approval = requested(&mut events).await;
    assert_eq!(approval.tool, "effect");
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    fixture
        .harness
        .decide(&approval.id, Decision::Approve)
        .unwrap();
    let run = fixture
        .harness
        .wait_for_idle(&fixture.session)
        .await
        .unwrap();
    assert_eq!(run.status, "completed");
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert!(
        fixture.result().contains("\"ran\":true"),
        "{}",
        fixture.result()
    );

    let mut events = fixture.start(CATCHING);
    let approval = requested(&mut events).await;
    fixture
        .harness
        .decide(
            &approval.id,
            Decision::Deny {
                reason: Some("not this file".into()),
            },
        )
        .unwrap();
    fixture
        .harness
        .wait_for_idle(&fixture.session)
        .await
        .unwrap();
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    let result = fixture.result();
    assert!(result.contains("caught"), "{result}");
    assert!(
        result.contains(
            "Denied by the user: not this file. Do not retry this call unless the user asks."
        ),
        "{result}"
    );
}

#[tokio::test]
async fn cancelling_a_js_cell_waiting_for_approval_cancels_it() {
    let fixture = Fixture::new();
    let mut events = fixture.start(CATCHING);
    let approval = requested(&mut events).await;
    assert!(fixture.harness.cancel(&fixture.session));
    let run = fixture
        .harness
        .wait_for_idle(&fixture.session)
        .await
        .unwrap();
    assert_eq!(run.status, "cancelled");
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    // The aborted cell drops the gate, which records the cancellation.
    tokio::time::timeout(Duration::from_secs(5), async {
        while !fixture
            .harness
            .pending_approvals(&fixture.session)
            .unwrap()
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("approval left pending");
    let error = fixture
        .harness
        .decide(&approval.id, Decision::Approve)
        .unwrap_err();
    assert!(error.to_string().contains("already cancelled"), "{error}");
}

/// The idle limit bounds both an idle cell and a non-wait host call.
#[tokio::test]
async fn approval_wait_does_not_count_against_cell_or_call_deadlines() {
    let fixture = Fixture::with_idle_limit(Duration::from_secs(1));
    let mut events = fixture.start(CATCHING);
    let approval = requested(&mut events).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    fixture
        .harness
        .decide(&approval.id, Decision::Approve)
        .unwrap();
    let run = fixture
        .harness
        .wait_for_idle(&fixture.session)
        .await
        .unwrap();
    assert_eq!(run.status, "completed");
    assert!(
        fixture.result().contains("\"ran\":true"),
        "{}",
        fixture.result()
    );
}

async fn send(stdin: &mut ChildStdin, id: i32, method: &str, params: Value) {
    let mut bytes =
        serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .unwrap();
    bytes.push(b'\n');
    stdin.write_all(&bytes).await.unwrap();
    stdin.flush().await.unwrap();
}
async fn next(stdout: &mut BufReader<ChildStdout>) -> Value {
    let mut line = String::new();
    let count = tokio::time::timeout(Duration::from_secs(20), stdout.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert!(count > 0, "runner closed stdout");
    serde_json::from_str(&line).unwrap()
}
async fn reply(stdout: &mut BufReader<ChildStdout>, id: i32) -> Value {
    loop {
        let record = next(stdout).await;
        if record["id"] == id {
            return record;
        }
    }
}
async fn event(stdout: &mut BufReader<ChildStdout>, kind: &str) -> Value {
    loop {
        let record = next(stdout).await;
        if record["method"] == "event" && record["params"]["type"] == kind {
            return record["params"].clone();
        }
    }
}

#[tokio::test]
async fn stdio_lists_and_decides_approvals() {
    let temp = tempfile::tempdir().unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_bubblepaw"))
        .args(["--provider", "demo", "--workspace"])
        .arg(temp.path())
        .arg("--db")
        .arg(temp.path().join("archive.sqlite"))
        .args(["--read-only", "--confirm", "list", "stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    send(&mut stdin, 1, "session.open", json!({"title":"approvals"})).await;
    let session = reply(&mut stdout, 1).await["result"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    send(
        &mut stdin,
        2,
        "run.start",
        json!({"session_id":session,"text":"Run the demo"}),
    )
    .await;
    let requested = event(&mut stdout, "approval_requested").await;
    assert_eq!(requested["approval"]["tool"], "list");
    assert_eq!(requested["approval"]["state"], "pending");
    send(
        &mut stdin,
        3,
        "approval.list",
        json!({"session_id":session}),
    )
    .await;
    let listed = reply(&mut stdout, 3).await["result"].clone();
    assert_eq!(listed.as_array().unwrap().len(), 1, "{listed}");
    let id = listed[0]["id"].as_str().unwrap().to_owned();
    assert_eq!(id, requested["approval"]["id"]);
    send(
        &mut stdin,
        4,
        "approval.decide",
        json!({"approval_id":id,"decision":"approve"}),
    )
    .await;
    assert_eq!(reply(&mut stdout, 4).await["result"]["state"], "approved");
    send(
        &mut stdin,
        5,
        "approval.decide",
        json!({"approval_id":id,"decision":"deny","reason":"late"}),
    )
    .await;
    let again = reply(&mut stdout, 5).await;
    assert!(
        again["error"]["message"]
            .as_str()
            .unwrap()
            .contains("already approved"),
        "{again}"
    );
    let finished = event(&mut stdout, "run_finished").await;
    assert_eq!(finished["status"], "completed", "{finished}");
    send(
        &mut stdin,
        6,
        "approval.list",
        json!({"session_id":session}),
    )
    .await;
    assert_eq!(reply(&mut stdout, 6).await["result"], json!([]));
    send(&mut stdin, 7, "shutdown", json!({})).await;
    reply(&mut stdout, 7).await;
    drop(stdin);
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
}

#[test]
fn one_shot_run_denies_what_would_wait() {
    let temp = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_bubblepaw"))
        .args(["--provider", "demo", "--json", "--workspace"])
        .arg(temp.path())
        .arg("--db")
        .arg(temp.path().join("archive.sqlite"))
        .args(["--read-only", "--confirm", "list", "run", "Run the demo"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("no client to approve; use --approve-all"),
        "{stdout}"
    );
    assert!(stdout.contains("\"state\":\"denied\""), "{stdout}");
}
