//! Scripted local coverage for sync `exec` steering inside CodeMode cells.
//! No model, provider or network calls: a scripted provider answers each user
//! JSON cell with one `js` call and records every request it receives.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, Role,
    RunOptions, Store, TextCallback, ToolCall, ToolContext, ToolRegistry,
};
use bubblepaw_runner::{
    artifacts::ArtifactStore,
    codemode::CodeMode,
    config::ExecConfig,
    tools::{ProcessTools, WorkspaceTools},
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

/// Writes five bytes, then keeps the sync wait open until steered or cancelled.
#[cfg(windows)]
const HOLD: &str = "[Console]::Out.Write('ready'); Start-Sleep -Seconds 30";
#[cfg(not(windows))]
const HOLD: &str = "printf ready; sleep 30";
/// Byte length of "ready", the barrier for output produced inside the sync wait.
const READY_BYTES: u64 = 5;

/// Answers a JSON user message carrying `code` with one `js` call, then settles.
/// Every request is recorded so tests can inspect what the model would receive.
struct Scripted {
    requests: Mutex<Vec<ModelRequest>>,
}
#[async_trait]
impl Provider for Scripted {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let index = request
            .messages
            .iter()
            .rposition(|message| message.role == Role::User)
            .unwrap();
        let answered = request.messages[index + 1..]
            .iter()
            .any(|message| message.role == Role::Tool);
        let response = match serde_json::from_str::<Value>(&request.messages[index].content) {
            Ok(arguments) if arguments.get("code").is_some() && !answered => ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "js".into(),
                    arguments,
                }],
                ..Default::default()
            },
            _ => ModelResponse {
                text: "settled".into(),
                ..Default::default()
            },
        };
        self.requests.lock().unwrap().push(request);
        Ok(response)
    }
}

struct Fixture {
    _workspace: tempfile::TempDir,
    _data: tempfile::TempDir,
    session: String,
    harness: Harness,
    provider: Arc<Scripted>,
    registry: ToolRegistry,
    processes: Arc<ProcessTools>,
    manager: Arc<CodeMode>,
}
impl Fixture {
    /// A missed explicit release reaches the sync deadline instead of hanging.
    fn new(sync_secs: u64) -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let processes = Arc::new(
            ProcessTools::with_config(
                WorkspaceTools::new(workspace.path()).unwrap(),
                ExecConfig {
                    sync_timeout_secs: sync_secs,
                    ..ExecConfig::default()
                },
                data.path(),
            )
            .unwrap(),
        );
        let mut registry = ToolRegistry::new();
        processes.register(&mut registry).unwrap();
        processes.register_wait(&mut registry).unwrap();
        registry.show_always(["exec", "poll", "wait", "jobs"]);
        let artifacts = Arc::new(
            ArtifactStore::new(workspace.path(), workspace.path().join("artifacts")).unwrap(),
        );
        let manager = Arc::new(CodeMode::new(workspace.path().to_owned(), artifacts).unwrap());
        manager.register(&mut registry).unwrap();
        let store = Store::in_memory().unwrap();
        let session = store.create_session("exec steering").unwrap().id;
        let provider = Arc::new(Scripted {
            requests: Mutex::default(),
        });
        let harness = Harness::new(
            store,
            provider.clone(),
            registry.clone(),
            HarnessConfig {
                execution_mode: bubblepaw_core::ExecutionMode::Js,
                ..Default::default()
            },
        );
        Self {
            _workspace: workspace,
            _data: data,
            session,
            harness,
            provider,
            registry,
            processes,
            manager,
        }
    }
    fn start(&self, code: &str) {
        self.harness
            .start(
                &self.session,
                json!({"code": code}).to_string(),
                RunOptions::default(),
            )
            .unwrap();
    }
    async fn settle(&self) {
        tokio::time::timeout(
            Duration::from_secs(60),
            self.harness.wait_for_idle(&self.session),
        )
        .await
        .expect("run settle deadline")
        .unwrap();
    }
    async fn until_env(&self, what: &str, ready: impl Fn(&Value) -> bool) {
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if ready(&self.manager.status(&self.session)["environments"][0]) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("environment state deadline: {what}"));
    }
    /// Barrier: the sync exec is foreground and has captured `bytes` of stdout.
    async fn foreground_handle(&self, bytes: u64) -> String {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let jobs = self.processes.jobs(&self.session).unwrap();
                let found = jobs["jobs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|job| job["foreground"] == true && job["stdoutBytes"] == bytes)
                    .map(|job| job["handle"].as_str().unwrap().to_owned());
                if let Some(handle) = found {
                    return handle;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("foreground output barrier")
    }
    fn environment(&self) -> Value {
        self.manager.status(&self.session)["environments"][0].clone()
    }
    async fn call(&self, name: &str, input: Value) -> Value {
        call(&self.registry, &self.session, name, input).await
    }
    async fn cancel_process(&self, handle: &str) {
        self.call("cancel", json!({"handle": handle})).await;
        let done = self
            .call("wait", json!({"handle": handle, "timeoutMs": 60000}))
            .await;
        assert_eq!(done["status"], "cancelled", "{done}");
    }
    /// Explicit teardown: cancel any open run (which also stops owned commands),
    /// then stop the environment and process hosts.
    async fn close(self) {
        self.harness.cancel(&self.session);
        let _ = tokio::time::timeout(
            Duration::from_secs(60),
            self.harness.wait_for_idle(&self.session),
        )
        .await;
        self.manager.shutdown().await;
        self.processes.shutdown().await;
    }
}

fn context(owner: &str) -> ToolContext {
    ToolContext {
        session_id: owner.into(),
        run_id: "run".into(),
        invocation_id: "invocation".into(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    }
}
async fn call(registry: &ToolRegistry, owner: &str, name: &str, input: Value) -> Value {
    let result = registry
        .get(name)
        .unwrap()
        .invoke(context(owner), input)
        .await
        .unwrap();
    assert!(!result.is_error, "{}", result.content);
    serde_json::from_str(&result.content).unwrap()
}
/// The JS reply before any state-notice suffix.
fn reply(content: &str) -> Value {
    serde_json::from_str(content.split("\n[---]\n").next().unwrap()).unwrap()
}
/// The first tool reply in a captured request, with its message position.
fn tool_reply(request: &ModelRequest) -> (usize, Value) {
    request
        .messages
        .iter()
        .enumerate()
        .find(|(_, message)| message.role == Role::Tool)
        .map(|(index, message)| (index, reply(&message.content)))
        .expect("tool reply")
}
fn input_position(request: &ModelRequest, text: &str) -> usize {
    request
        .messages
        .iter()
        .position(|message| message.role == Role::User && message.content == text)
        .expect("steer input")
}
fn exec_cell() -> String {
    format!(
        "const job = await tools.exec({{command:{}}}); return job;",
        json!(HOLD)
    )
}
fn concurrent_cell() -> String {
    format!(
        "const pending = tools.exec({{command:{}}}); const timer = wait({{ms:60000}}); const [job, timed] = await Promise.all([pending, timer]); return {{job, timed}};",
        json!(HOLD)
    )
}

#[tokio::test]
async fn explicit_wait_release_returns_ordinary_js_result_and_command_keeps_running() {
    let f = Fixture::new(20);
    f.start(&exec_cell());
    let handle = f.foreground_handle(READY_BYTES).await;
    f.harness.steer(&f.session, "new input").unwrap();
    assert!(f.registry.interrupt_waits(&f.session) > 0);
    f.settle().await;
    {
        let requests = f.provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let (position, result) = tool_reply(&requests[1]);
        assert_eq!(result["status"], "backgrounded");
        assert_eq!(result["reason"], "steer");
        assert_eq!(result["handle"], handle.as_str());
        assert_eq!(result["stdout"], "ready");
        assert!(
            result.get("execution_id").is_none(),
            "cell returned a background receipt: {result}"
        );
        assert!(position < input_position(&requests[1], "new input"));
    }
    f.until_env("cell completed normally", |e| e["status"] == "idle")
        .await;
    let jobs = f.processes.jobs(&f.session).unwrap();
    let job = jobs["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|job| job["handle"] == handle.as_str())
        .unwrap();
    assert_eq!(job["status"], "running");
    assert!(job["backgrounded"] == true && job["foreground"] == false);
    let waited = f
        .call("wait", json!({"handle": handle, "timeoutMs": 0}))
        .await;
    assert_eq!(waited["status"], "timeout");
    let polled = f.call("poll", json!({"handle": handle})).await;
    assert_eq!(polled["stdout"], "ready");
    f.cancel_process(&handle).await;
    f.close().await;
}

#[tokio::test]
async fn cell_awaiting_sync_exec_and_timer_backgrounds_with_receipt_then_wake_completes_it() {
    let f = Fixture::new(20);
    f.start(&concurrent_cell());
    let handle = f.foreground_handle(READY_BYTES).await;
    f.until_env("timer registered", |e| e["time_waiting"] == 1)
        .await;
    f.harness.steer(&f.session, "new input").unwrap();
    assert!(f.registry.interrupt_waits(&f.session) > 0);
    f.settle().await;
    let execution_id;
    {
        let requests = f.provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let (position, receipt) = tool_reply(&requests[1]);
        assert_eq!(receipt["status"], "background");
        assert_eq!(receipt["name"], "main");
        // Explicit release interrupts the sync exec and timer in an order that the
        // receipt snapshot may observe either way, so only the pending timer and
        // an honest background status are fixed.
        let waiting = receipt["waiting"].as_u64().unwrap();
        assert!((1..=2).contains(&waiting), "{receipt}");
        assert_eq!(receipt["time_waiting"], 1);
        assert!(receipt["execution_id"].is_string(), "{receipt}");
        assert!(position < input_position(&requests[1], "new input"));
        execution_id = receipt["execution_id"].clone();
    }
    assert_eq!(f.environment()["status"], "background");
    f.manager
        .control(&f.session, json!({"action": "wake", "environment": "main"}))
        .unwrap();
    f.until_env("woken continuation idle", |e| e["status"] == "idle")
        .await;
    let inspected = f
        .manager
        .control(
            &f.session,
            json!({"action": "inspect", "environment": "main"}),
        )
        .unwrap();
    assert_eq!(inspected["result"]["timed"]["status"], "woken");
    assert_eq!(inspected["result"]["job"]["status"], "backgrounded");
    assert_eq!(inspected["result"]["job"]["reason"], "steer");
    assert_eq!(inspected["result"]["job"]["handle"], handle.as_str());
    assert_eq!(inspected["result_execution_id"], execution_id);
    f.cancel_process(&handle).await;
    f.close().await;
}

#[tokio::test]
async fn cancelling_backgrounded_concurrent_cell_releases_timer_and_stops_owned_command() {
    let f = Fixture::new(20);
    f.start(&concurrent_cell());
    let handle = f.foreground_handle(READY_BYTES).await;
    f.until_env("timer registered", |e| e["time_waiting"] == 1)
        .await;
    f.harness.steer(&f.session, "new input").unwrap();
    assert!(f.registry.interrupt_waits(&f.session) > 0);
    f.settle().await;
    assert_eq!(f.environment()["status"], "background");
    f.harness.cancel(&f.session);
    f.until_env("timer released and cell cancelled", |e| {
        e["time_waiting"] == 0
            && e["waiting"] == 0
            && e["status"] == "idle"
            && e["generation"]
                .as_u64()
                .is_some_and(|generation| generation > 1)
    })
    .await;
    let done = f
        .call("wait", json!({"handle": handle, "timeoutMs": 60000}))
        .await;
    assert_eq!(done["status"], "cancelled", "{done}");
    f.close().await;
}

#[tokio::test]
async fn user_background_returns_ordinary_js_result_without_cell_receipt() {
    let f = Fixture::new(20);
    f.start(&exec_cell());
    let handle = f.foreground_handle(READY_BYTES).await;
    let snapshot = f.processes.background(&f.session, &handle).unwrap();
    assert_eq!(snapshot["reason"], "user");
    f.settle().await;
    {
        let requests = f.provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let (_, result) = tool_reply(&requests[1]);
        assert_eq!(result["status"], "backgrounded");
        assert_eq!(result["reason"], "user");
        assert!(result.get("execution_id").is_none(), "{result}");
    }
    f.until_env("cell completed normally", |e| e["status"] == "idle")
        .await;
    f.cancel_process(&handle).await;
    f.close().await;
}

#[tokio::test]
async fn js_cell_classifies_background_and_zero_or_short_yield_as_running_without_reason() {
    let f = Fixture::new(20);
    f.start(&format!(
        "const started = await Promise.all([\
         tools.exec({{command:{hold}, background:true}}),\
         tools.exec({{command:{hold}, yieldMs:0}}),\
         tools.exec({{command:{hold}, yieldMs:50}})]);\
         const report = started.map(job => ({{status: job.status, handle: job.handle, reason: job.reason === undefined ? null : job.reason}}));\
         for (const job of started) await tools.cancel({{handle: job.handle}});\
         const ends = [];\
         for (const job of started) ends.push((await wait({{handle: job.handle, timeoutMs: 60000}})).status);\
         return {{report, ends}};",
        hold = json!(HOLD)
    ));
    f.settle().await;
    {
        let requests = f.provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let (_, result) = tool_reply(&requests[1]);
        let report = result["report"].as_array().unwrap();
        assert_eq!(report.len(), 3, "{result}");
        for job in report {
            assert_eq!(job["status"], "running", "{result}");
            assert_eq!(job["reason"], Value::Null, "{result}");
            assert!(job["handle"].is_string(), "{result}");
        }
        assert_eq!(
            result["ends"],
            json!(["cancelled", "cancelled", "cancelled"])
        );
    }
    // Every job the cell started is terminal, so nothing is left running.
    let jobs = f.processes.jobs(&f.session).unwrap();
    let jobs = jobs["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 3);
    assert!(
        jobs.iter().all(|job| job["status"] == "cancelled"),
        "{jobs:?}"
    );
    f.close().await;
}

#[tokio::test]
async fn cancelling_cell_during_sync_exec_stops_command_and_resets_environment() {
    let f = Fixture::new(20);
    f.start(&exec_cell());
    let handle = f.foreground_handle(READY_BYTES).await;
    f.harness.cancel(&f.session);
    f.settle().await;
    f.until_env("cell cancelled and idle", |e| {
        e["status"] == "idle"
            && e["generation"]
                .as_u64()
                .is_some_and(|generation| generation > 1)
    })
    .await;
    let done = f
        .call("wait", json!({"handle": handle, "timeoutMs": 60000}))
        .await;
    assert_eq!(done["status"], "cancelled", "{done}");
    f.close().await;
}

#[path = "exec_codemode/model_description.rs"]
mod model_description;
