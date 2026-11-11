//! Native batches run in provider order: a script written in a batch can be
//! executed in that same batch, background jobs started in one batch are joined
//! by waits in a later batch, and a failing call leaves later calls to run.
use anyhow::{Result, bail};
use async_trait::async_trait;
use bubblepaw_core::{
    ApprovalMode, ApprovalPolicy, CancellationToken, Harness, HarnessConfig, Message, ModelRequest,
    ModelResponse, Provider, Role, RunOptions, Store, TextCallback, ToolCall, ToolRegistry,
};
use bubblepaw_runner::{
    config::ExecConfig,
    tools::{ProcessTools, WorkspaceTools, register_native_tools},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

type Round = Box<dyn Fn(&ModelRequest) -> ModelResponse + Send + Sync>;

/// One scripted reply per provider request. A round sees the requests so far,
/// which is how a later batch learns the process handles that earlier calls returned.
struct ScriptedRounds {
    rounds: Vec<Round>,
    requests: Mutex<Vec<ModelRequest>>,
}

#[async_trait]
impl Provider for ScriptedRounds {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let round = self.requests.lock().unwrap().len();
        let Some(script) = self.rounds.get(round) else {
            bail!("unexpected extra provider request");
        };
        let response = script(&request);
        self.requests.lock().unwrap().push(request);
        Ok(response)
    }
}

fn reply(script: impl Fn(&ModelRequest) -> ModelResponse + Send + Sync + 'static) -> Round {
    Box::new(script)
}

fn calls(calls: Vec<ToolCall>) -> Round {
    reply(move |_| ModelResponse {
        tool_calls: calls.clone(),
        ..Default::default()
    })
}

fn text(text: &'static str) -> Round {
    reply(move |_| ModelResponse {
        text: text.into(),
        ..Default::default()
    })
}

fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    }
}

/// Runs the rounds as one prompt; every provider request is recorded.
async fn run(root: &std::path::Path, rounds: Vec<Round>) -> Arc<ScriptedRounds> {
    let store = Store::in_memory().unwrap();
    let session = store.create_session("native batch exec").unwrap().id;
    let mut tools = ToolRegistry::new();
    let processes = register_native_tools(&mut tools, root, false).unwrap();
    run_with_tools(store, &session, tools, processes, rounds).await
}

async fn run_with_tools(
    store: Store,
    session: &str,
    tools: ToolRegistry,
    processes: Arc<ProcessTools>,
    rounds: Vec<Round>,
) -> Arc<ScriptedRounds> {
    let provider = Arc::new(ScriptedRounds {
        rounds,
        requests: Mutex::default(),
    });
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        tools,
        HarnessConfig {
            approval: ApprovalPolicy {
                confirm: ApprovalMode::Allow,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let outcome = harness
        .prompt(session, "Run the batch", RunOptions::default())
        .await
        .unwrap();
    processes.shutdown().await;
    assert_eq!(outcome.status, "completed", "{:?}", outcome.error);
    assert!(store.pending_effects(session).unwrap().is_empty());
    provider
}

/// Tool results answering the batch that the request follows, in recorded order.
fn result_ids(request: &ModelRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .rev()
        .take_while(|m| m.role != Role::Assistant)
        .filter(|m| m.role == Role::Tool)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|m| m.tool_call_id.clone().unwrap())
        .collect()
}

fn result_of<'a>(request: &'a ModelRequest, id: &str) -> &'a Message {
    request
        .messages
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some(id))
        .unwrap_or_else(|| panic!("no tool result for {id}"))
}

fn json_of(message: &Message) -> Value {
    serde_json::from_str(&message.content).unwrap()
}

#[cfg(windows)]
const ECHO_SCRIPT: &str = "Write-Output 'script-ran'\n";
#[cfg(not(windows))]
const ECHO_SCRIPT: &str = "printf 'script-ran\\n'\n";

fn script_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.ps1")
    } else {
        format!("{stem}.sh")
    }
}

/// Windows names the .ps1 as argv after powershell.exe, so no shell string is parsed;
/// Unix runs the script with sh.
fn script_command(stem: &str) -> Value {
    script_path_command(std::path::Path::new(&script_name(stem)))
}

fn script_path_command(path: &std::path::Path) -> Value {
    #[cfg(windows)]
    {
        let path = path.to_str().unwrap();
        // Windows PowerShell's -File accepts ordinary absolute drive/UNC paths.
        let path = if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{unc}")
        } else {
            path.strip_prefix(r"\\?\").unwrap_or(path).to_owned()
        };
        json!({"parts":["powershell.exe -NoProfile -ExecutionPolicy Bypass -File ", ""], "values":[path]})
    }
    #[cfg(not(windows))]
    json!({"parts":["sh ",""],"values":[path]})
}

/// A job that sleeps briefly, prints its name and exits with `code`.
fn job_script(name: &str, code: u8) -> String {
    if cfg!(windows) {
        format!("Start-Sleep -Seconds 1\nWrite-Output '{name}'\nexit {code}\n")
    } else {
        format!("sleep 1\nprintf '{name}\\n'\nexit {code}\n")
    }
}

#[tokio::test]
async fn scripts_written_to_both_absolute_scratchpads_run_in_the_same_native_batch() {
    let root = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let workspace = WorkspaceTools::new(root.path()).unwrap();
    workspace.prepare_scratchpads(data.path()).unwrap();
    let store = Store::in_memory().unwrap();
    let session = store.create_session("native scratch batch").unwrap().id;
    let scratch = workspace.scratchpads(&store, &session).unwrap();
    let chat_script = scratch.chat.join(script_name("chat run"));
    let project_script = scratch.workspace.join(script_name("project run"));
    let chat_source = ECHO_SCRIPT.replace("script-ran", "chat-script-ran");
    let project_source = ECHO_SCRIPT.replace("script-ran", "project-script-ran");
    assert!(chat_script.is_absolute() && project_script.is_absolute());
    let project = root.path().canonicalize().unwrap();
    assert!(!chat_script.starts_with(&project) && !project_script.starts_with(&project));
    let mut tools = ToolRegistry::new();
    workspace.register(&mut tools, false).unwrap();
    let processes =
        Arc::new(ProcessTools::with_config(workspace, ExecConfig::default(), data.path()).unwrap());
    processes.register(&mut tools).unwrap();
    processes.register_wait(&mut tools).unwrap();
    let provider = run_with_tools(
        store,
        &session,
        tools,
        processes,
        vec![
            calls(vec![
                call(
                    "write-chat",
                    "write",
                    json!({"path":chat_script,"content":chat_source}),
                ),
                call(
                    "exec-chat",
                    "exec",
                    json!({"command":script_path_command(&chat_script)}),
                ),
                call(
                    "write-project",
                    "write",
                    json!({"path":project_script,"content":project_source}),
                ),
                call(
                    "exec-project",
                    "exec",
                    json!({"command":script_path_command(&project_script)}),
                ),
            ]),
            text("Both scratch scripts ran"),
        ],
    )
    .await;
    assert_eq!(std::fs::read_to_string(&chat_script).unwrap(), chat_source);
    assert_eq!(
        std::fs::read_to_string(&project_script).unwrap(),
        project_source
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        result_ids(&requests[1]),
        ["write-chat", "exec-chat", "write-project", "exec-project"]
    );
    for id in ["write-chat", "write-project"] {
        let write = result_of(&requests[1], id);
        assert!(!write.is_error, "{}", write.content);
    }
    for (id, expected) in [
        ("exec-chat", "chat-script-ran\n"),
        ("exec-project", "project-script-ran\n"),
    ] {
        let exec = result_of(&requests[1], id);
        assert!(!exec.is_error, "{}", exec.content);
        let exec = json_of(exec);
        assert_eq!(exec["status"], "exited");
        assert_eq!(exec["exitCode"], 0);
        assert_eq!(
            exec["stdout"].as_str().unwrap().replace("\r\n", "\n"),
            expected
        );
        assert_eq!(exec["stderr"], "");
    }
}

#[tokio::test]
async fn two_background_execs_are_joined_by_one_wait_batch() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join(script_name("job-a")),
        job_script("job-a", 0),
    )
    .unwrap();
    std::fs::write(
        root.path().join(script_name("job-b")),
        job_script("job-b", 3),
    )
    .unwrap();
    let provider = run(
        root.path(),
        vec![
            calls(vec![
                call(
                    "start-a",
                    "exec",
                    json!({"command": script_command("job-a"), "background": true}),
                ),
                call(
                    "start-b",
                    "exec",
                    json!({"command": script_command("job-b"), "background": true}),
                ),
            ]),
            reply(|request| {
                let handle = |id: &str| {
                    json_of(result_of(request, id))["handle"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                };
                ModelResponse {
                    tool_calls: vec![
                        call("join-a", "wait", json!({"handle": handle("start-a")})),
                        call("join-b", "wait", json!({"handle": handle("start-b")})),
                    ],
                    ..Default::default()
                }
            }),
            text("Both jobs joined"),
        ],
    )
    .await;
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(result_ids(&requests[1]), ["start-a", "start-b"]);
    assert_eq!(result_ids(&requests[2]), ["join-a", "join-b"]);
    let mut handles = Vec::new();
    for id in ["start-a", "start-b"] {
        let started = result_of(&requests[1], id);
        assert!(!started.is_error, "{}", started.content);
        let started = json_of(started);
        assert_eq!(started["status"], "running");
        handles.push(started["handle"].as_str().unwrap().to_owned());
    }
    assert_ne!(handles[0], handles[1]);
    for (id, handle, exit_code) in [("join-a", &handles[0], 0), ("join-b", &handles[1], 3)] {
        let joined = result_of(&requests[2], id);
        assert!(!joined.is_error, "{}", joined.content);
        let joined = json_of(joined);
        assert_eq!(joined["handle"].as_str().unwrap(), handle.as_str());
        assert_eq!(joined["status"], "exited");
        assert_eq!(joined["exitCode"], exit_code);
    }
}

#[tokio::test]
async fn failed_read_mid_batch_does_not_stop_later_calls() {
    let root = tempfile::tempdir().unwrap();
    let provider = run(
        root.path(),
        vec![
            calls(vec![
                call(
                    "write",
                    "write",
                    json!({"path": script_name("run"), "content": ECHO_SCRIPT}),
                ),
                call("missing", "read", json!({"path": "missing.txt"})),
                call("exec", "exec", json!({"command": script_command("run")})),
            ]),
            text("Done"),
        ],
    )
    .await;
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(result_ids(&requests[1]), ["write", "missing", "exec"]);
    assert!(!result_of(&requests[1], "write").is_error);
    assert!(result_of(&requests[1], "missing").is_error);
    let exec = result_of(&requests[1], "exec");
    assert!(!exec.is_error, "{}", exec.content);
    let exec = json_of(exec);
    assert_eq!(exec["exitCode"], 0);
    assert!(exec["stdout"].as_str().unwrap().contains("script-ran"));
}

#[tokio::test]
async fn unknown_tool_mid_batch_does_not_stop_later_calls() {
    let root = tempfile::tempdir().unwrap();
    let provider = run(
        root.path(),
        vec![
            calls(vec![
                call(
                    "write",
                    "write",
                    json!({"path": script_name("run"), "content": ECHO_SCRIPT}),
                ),
                call("unknown", "no_such_tool", json!({})),
                call("exec", "exec", json!({"command": script_command("run")})),
            ]),
            text("Done"),
        ],
    )
    .await;
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(result_ids(&requests[1]), ["write", "unknown", "exec"]);
    let unknown = result_of(&requests[1], "unknown");
    assert!(unknown.is_error);
    assert!(
        unknown.content.contains("no_such_tool"),
        "{}",
        unknown.content
    );
    let exec = result_of(&requests[1], "exec");
    assert!(!exec.is_error, "{}", exec.content);
    assert_eq!(json_of(exec)["exitCode"], 0);
}
