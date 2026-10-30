//! Actual host routes with a loopback scripted provider and a local fake MCP.
use axum::{Json, Router, routing::post};
use serde_json::{Value, json};
use std::{
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    process::{Child, ChildStdin, ChildStdout, Command},
    task::JoinHandle,
};

#[cfg(feature = "web")]
#[path = "support/web_host.rs"]
mod web_host;

struct ScriptedEndpoint {
    base: String,
    requests: Arc<Mutex<Vec<Value>>>,
    task: JoinHandle<()>,
}
impl ScriptedEndpoint {
    async fn start() -> Self {
        Self::with_answer("scripted answer").await
    }
    async fn with_answer(answer: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let app = Router::new().route("/chat/completions", post(move |Json(body): Json<Value>| {
            seen.lock().unwrap().push(body);
            async move { Json(json!({"choices":[{"message":{"role":"assistant","content":answer},"finish_reason":"stop"}]})) }
        }));
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base,
            requests,
            task,
        }
    }
    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}
impl Drop for ScriptedEndpoint {
    fn drop(&mut self) {
        self.task.abort();
    }
}
fn config(base: &str, root: &Path, output_limit: usize) -> String {
    let command = toml::Value::String(env!("CARGO_BIN_EXE_bubblepaw-runner-fake-mcp").into());
    let arguments = serde_json::to_string(&[
        "--pid-file".to_string(),
        root.join("mcp.pid").to_string_lossy().into_owned(),
    ])
    .unwrap();
    format!(
        "[agent]\nexecution_mode='js'\n[model]\nmain='fixture/test'\n[providers.fixture]\nkind='chat-completions'\nbase_url='{base}'\nkey_env='BP_LIVE_CONFIG_ROUTE_KEY'\nmodels=[{{id='test'}}]\n[approvals]\nconfirm='allow'\n[agents]\nauto_wake=false\n[exec]\nmax_output_bytes={output_limit}\n[mcp.servers.fixture]\ncommand={command}\nargs={arguments}\napproval='none'\n"
    )
}
fn host(root: &Path, file: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bubblepaw"));
    command
        .arg("--config")
        .arg(file)
        .arg("--workspace")
        .arg(root)
        .arg("--data-dir")
        .arg(root.join("state"))
        .arg("--no-auto-summarize")
        .env("BP_LIVE_CONFIG_ROUTE_KEY", "synthetic-loopback-key")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for (name, _) in std::env::vars() {
        if name.starts_with("BUBBLEPAW_") {
            command.env_remove(name);
        }
    }
    command
}
struct StdioHost {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next_id: u64,
    settled: std::collections::HashSet<String>,
}
impl StdioHost {
    fn start(root: &Path, file: &Path) -> Self {
        let mut child = host(root, file).arg("stdio").spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            input,
            output,
            next_id: 1,
            settled: Default::default(),
        }
    }
    async fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.input
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        self.input.flush().await.unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let mut line = String::new();
                assert!(
                    self.output.read_line(&mut line).await.unwrap() > 0,
                    "host exited before replying to {method}"
                );
                let record: Value = serde_json::from_str(&line).unwrap();
                if record["params"]["type"] == "session_settled"
                    && let Some(session) = record["params"]["session_id"].as_str()
                {
                    self.settled.insert(session.into());
                }
                if record["id"] == id {
                    assert!(record.get("error").is_none(), "{record}");
                    return record["result"].clone();
                }
            }
        })
        .await
        .unwrap()
    }
    async fn state(&mut self, session: &str) -> Value {
        self.rpc("session.state", json!({"session_id":session}))
            .await
    }
    async fn turn(&mut self, session: &str, script: Option<&str>) -> Value {
        self.settled.remove(session);
        let mut params = json!({"session_id":session,"text":"route fixture turn"});
        if let Some(script) = script {
            params["script"] = json!(script);
        }
        let started = self.rpc("run.start", params).await;
        let run = started["id"].clone();
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let state = self.state(session).await;
                let current = state["runs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|saved| saved["id"] == run)
                    .unwrap();
                if current["status"] != "running" && self.settled.contains(session) {
                    assert_eq!(current["status"], "completed", "{current}");
                    return state;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }
    async fn shutdown(mut self) {
        self.rpc("shutdown", json!({})).await;
        assert!(
            tokio::time::timeout(Duration::from_secs(5), self.child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
}
fn notices(state: &Value) -> Vec<String> {
    state["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_str())
        .filter(|content| content.contains("<harness kind=\"live_config:"))
        .map(str::to_owned)
        .collect()
}
fn changed(report: &Value, text: &str) -> bool {
    report["changed"]
        .as_array()
        .unwrap()
        .iter()
        .any(|change| change.as_str().unwrap().contains(text))
}
fn assert_unchanged(report: &Value) {
    assert_eq!(report["applied"], true, "{report}");
    assert_eq!(report["changed"], json!([]));
    assert_eq!(report["errors"], json!([]));
}
fn pid(root: &Path) -> String {
    std::fs::read_to_string(root.join("mcp.pid")).unwrap()
}

#[tokio::test]
async fn stdio_rpc_slash_and_reload_tool_share_diffs_and_preserve_invalid_config() {
    let provider = ScriptedEndpoint::start().await;
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("config.toml");
    std::fs::write(&file, config(&provider.base, root.path(), 8192)).unwrap();
    let mut host = StdioHost::start(root.path(), &file);
    let opened = host.rpc("session.open", json!({})).await;
    let session = opened["id"].as_str().unwrap();
    let before_pid = pid(root.path());
    std::fs::write(&file, config(&provider.base, root.path(), 4096)).unwrap();
    let report = host.rpc("config.reload", json!({})).await;
    assert_eq!(report["applied"], true);
    assert!(changed(&report, "exec updated"));
    assert_eq!(report["restart_needed"], false);
    assert_eq!(
        pid(root.path()),
        before_pid,
        "unchanged MCP keeps its generation"
    );
    for method in ["run.start", "run.steer", "run.follow_up"] {
        let slash = host
            .rpc(method, json!({"session_id":session,"text":"/reload"}))
            .await;
        assert_unchanged(&slash["reload"]);
    }
    assert_eq!(provider.count(), 0, "slash commands never call a model");
    assert!(
        host.state(session).await["runs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let first = host.turn(session, None).await;
    assert_eq!(notices(&first).len(), 1);
    assert!(notices(&first)[0].contains("exec updated"));
    assert_unchanged(&host.rpc("config.reload", json!({})).await);
    let second = host.turn(session, None).await;
    assert_eq!(notices(&second), notices(&first));

    std::fs::write(&file, config(&provider.base, root.path(), 0)).unwrap();
    let rejected = host.rpc("config.reload", json!({})).await;
    assert_eq!(rejected["applied"], false);
    assert_eq!(rejected["active_main"], "fixture/test");
    assert!(
        rejected["errors"]
            .to_string()
            .contains("exec.max_output_bytes")
    );
    let invalid = host.turn(session, None).await;
    assert_eq!(notices(&invalid).len(), 2);
    assert!(notices(&invalid)[1].contains("exec.max_output_bytes"));
    let repeated = host.rpc("config.reload", json!({})).await;
    assert_eq!(repeated["applied"], false);
    let still_serving = host.turn(session, None).await;
    assert_eq!(notices(&still_serving), notices(&invalid));
    assert_eq!(
        provider.count(),
        4,
        "rejected config keeps the old provider serving"
    );

    std::fs::write(&file, config(&provider.base, root.path(), 4096)).unwrap();
    assert_unchanged(&host.rpc("config.reload", json!({})).await);
    let forced = host.rpc("config.reload", json!({"server":"fixture"})).await;
    assert!(changed(&forced, "server fixture restarted"));
    let rpc_pid = pid(root.path());
    assert_ne!(rpc_pid, before_pid);
    let slash = host
        .rpc(
            "run.start",
            json!({"session_id":session,"text":"/reload fixture"}),
        )
        .await;
    assert!(changed(&slash["reload"], "server fixture restarted"));
    assert_ne!(pid(root.path()), rpc_pid);
    assert_eq!(provider.count(), 4);

    // Edit inside the JS round so turn-start checking cannot apply it first.
    let edited = config(&provider.base, root.path(), 2048);
    let code = format!(
        "await tools.write({}); const r=await tools.reload({{server:'fixture'}}); if(!r.applied || !r.changed.some(x=>x==='exec updated') || !r.changed.some(x=>x.includes('server fixture restarted'))) throw Error(JSON.stringify(r)); return r;",
        json!({"path":"config.toml","content":edited})
    );
    let script_pid = pid(root.path());
    host.turn(session, Some(&code)).await;
    assert_ne!(pid(root.path()), script_pid);
    assert_eq!(
        provider.count(),
        4,
        "model-less tool route uses no provider"
    );
    let settled = host.turn(session, None).await;
    assert!(notices(&settled).last().unwrap().contains("exec updated"));
    let final_state = host.turn(session, None).await;
    assert_eq!(notices(&final_state), notices(&settled));
    host.shutdown().await;
}

#[cfg(feature = "web")]
async fn request(
    client: &reqwest::Client,
    base: &str,
    method: reqwest::Method,
    path: &str,
    body: Value,
) -> Value {
    let response = client
        .request(method, format!("{base}{path}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert!(status.is_success(), "{status}: {body}");
    body
}
#[cfg(feature = "web")]
async fn web_turn(client: &reqwest::Client, base: &str, session: &str) -> Value {
    let started = request(
        client,
        base,
        reqwest::Method::POST,
        &format!("/api/sessions/{session}/run"),
        json!({"text":"route fixture"}),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let state = request(
                client,
                base,
                reqwest::Method::GET,
                &format!("/api/sessions/{session}"),
                json!({}),
            )
            .await;
            if let Some(run) = state["runs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|run| run["id"] == started["id"])
                && run["status"] != "running"
                && state["active_run_id"].is_null()
            {
                assert_eq!(run["status"], "completed", "{run}");
                return state;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

#[cfg(feature = "web")]
#[tokio::test]
async fn http_reload_and_slash_share_host_config_diff_and_durable_notices() {
    let provider = ScriptedEndpoint::start().await;
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("config.toml");
    std::fs::write(&file, config(&provider.base, root.path(), 8192)).unwrap();
    let (mut child, base) = web_host::start(host(root.path(), &file)).await;
    let client = reqwest::Client::new();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            assert!(
                child.try_wait().unwrap().is_none(),
                "web host exited during startup"
            );
            if client
                .get(format!("{base}/api/providers"))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let opened = request(
        &client,
        &base,
        reqwest::Method::POST,
        "/api/sessions",
        json!({}),
    )
    .await;
    let session = opened["id"].as_str().unwrap();
    std::fs::write(&file, config(&provider.base, root.path(), 4096)).unwrap();
    let applied = request(
        &client,
        &base,
        reqwest::Method::POST,
        "/api/config/reload",
        json!({}),
    )
    .await;
    assert_eq!(applied["applied"], true);
    assert!(changed(&applied["diff"], "exec updated"));
    assert_eq!(applied["restart_needed"], false);
    let unchanged = request(
        &client,
        &base,
        reqwest::Method::POST,
        "/api/config/reload",
        json!({}),
    )
    .await;
    assert_unchanged(&unchanged["diff"]);
    let first = web_turn(&client, &base, session).await;
    assert_eq!(notices(&first).len(), 1);
    assert!(notices(&first)[0].contains("exec updated"));
    let second = web_turn(&client, &base, session).await;
    assert_eq!(notices(&second), notices(&first));

    std::fs::write(&file, config(&provider.base, root.path(), 0)).unwrap();
    let rejected = client
        .post(format!("{base}/api/config/reload"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), reqwest::StatusCode::BAD_REQUEST);
    let rejected: Value = rejected.json().await.unwrap();
    assert_eq!(rejected["diff"]["applied"], false);
    assert_eq!(rejected["diff"]["active_main"], "fixture/test");
    assert!(
        rejected["diff"]["errors"]
            .to_string()
            .contains("exec.max_output_bytes")
    );
    let invalid = web_turn(&client, &base, session).await;
    assert_eq!(notices(&invalid).len(), 2);
    let retained = web_turn(&client, &base, session).await;
    assert_eq!(notices(&retained), notices(&invalid));

    std::fs::write(&file, config(&provider.base, root.path(), 2048)).unwrap();
    let before_calls = provider.count();
    let before_pid = pid(root.path());
    let slash = request(
        &client,
        &base,
        reqwest::Method::POST,
        &format!("/api/sessions/{session}/run"),
        json!({"text":"/reload fixture"}),
    )
    .await;
    assert!(changed(&slash["reload"], "exec updated"));
    assert!(changed(&slash["reload"], "server fixture restarted"));
    assert_ne!(pid(root.path()), before_pid);
    assert_eq!(
        provider.count(),
        before_calls,
        "HTTP slash command makes no model request"
    );
    let final_state = web_turn(&client, &base, session).await;
    assert_eq!(notices(&final_state).len(), 3);
    assert!(notices(&final_state)[2].contains("exec updated"));
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}

#[tokio::test]
async fn stdio_manual_compact_rpc_and_slash_preserve_history_with_auto_disabled() {
    let provider = ScriptedEndpoint::with_answer("# Episode\nRetain original decisions.").await;
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("config.toml");
    std::fs::write(
        &file,
        format!(
            "{}\n[roles]\nsummary='fixture/test'\nchapter='summary'\n",
            config(&provider.base, root.path(), 8192)
        ),
    )
    .unwrap();
    let mut rpc_host = StdioHost::start(root.path(), &file);
    let mut sessions = Vec::new();
    for focus in [None, Some("Keep exact identifiers")] {
        let opened = rpc_host.rpc("session.open", json!({})).await;
        let session = opened["id"].as_str().unwrap();
        sessions.push(session.to_owned());
        rpc_host
            .turn(session, Some("text('completed first turn')"))
            .await;
        let state = rpc_host
            .turn(session, Some("text('completed recent turn')"))
            .await;
        let before_calls = provider.count();
        let mut params = json!({"session_id":session});
        if let Some(focus) = focus {
            params["focus"] = json!(focus);
        }
        let result = rpc_host.rpc("session.compact", params).await;
        assert!(
            result["notice"]
                .as_str()
                .unwrap()
                .contains("Context summarized"),
            "{result}"
        );
        assert_eq!(provider.count(), before_calls + 1);
        let summary_request = provider.requests.lock().unwrap().last().unwrap().clone();
        if let Some(focus) = focus {
            assert_fork_control_focus(&summary_request, focus);
        } else {
            assert!(
                !summary_request
                    .to_string()
                    .contains("For this manual compaction")
            );
        }
        let after = rpc_host.state(session).await;
        assert_initial_runtime_snapshot(&state["messages"], &after["messages"]);
        for method in ["run.start", "run.steer", "run.follow_up"] {
            let result = rpc_host
                .rpc(
                    method,
                    json!({"session_id":session,"text":"/compact preserve decisions"}),
                )
                .await;
            assert!(
                result["notice"]
                    .as_str()
                    .unwrap()
                    .contains("Nothing to compact")
            );
        }
        assert_eq!(provider.count(), before_calls + 1);
        assert_eq!(rpc_host.state(session).await["messages"], after["messages"]);
    }
    rpc_host.shutdown().await;
    let calls = provider.count();
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        host(root.path(), &file)
            .args(["run", "/compact keep decisions", "--session", &sessions[0]])
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        result["notice"]
            .as_str()
            .unwrap()
            .contains("Nothing to compact")
    );
    assert_eq!(provider.count(), calls);
}

#[cfg(feature = "web")]
#[tokio::test]
async fn web_manual_compact_message_routes_preserve_history() {
    let provider = ScriptedEndpoint::with_answer("# Episode\nRetain exact prior decisions.").await;
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("config.toml");
    std::fs::write(
        &file,
        format!(
            "{}\n[roles]\nsummary='fixture/test'\nchapter='summary'\n",
            config(&provider.base, root.path(), 8192)
        ),
    )
    .unwrap();
    let (mut child, base) = web_host::start(host(root.path(), &file)).await;
    let client = reqwest::Client::new();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            assert!(
                child.try_wait().unwrap().is_none(),
                "web host exited during startup"
            );
            if client
                .get(format!("{base}/api/providers"))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    for (route, text) in [
        ("run", "/compact"),
        ("steer", "/compact Keep exact identifiers"),
        ("followup", "/compact Keep failed attempts"),
    ] {
        let opened = request(
            &client,
            &base,
            reqwest::Method::POST,
            "/api/sessions",
            json!({}),
        )
        .await;
        let session = opened["id"].as_str().unwrap();
        web_turn(&client, &base, session).await;
        let before = web_turn(&client, &base, session).await;
        let calls = provider.count();
        let result = request(
            &client,
            &base,
            reqwest::Method::POST,
            &format!("/api/sessions/{session}/{route}"),
            json!({"text":text}),
        )
        .await;
        assert!(
            result["notice"]
                .as_str()
                .unwrap()
                .contains("Context summarized"),
            "{result}"
        );
        assert_eq!(provider.count(), calls + 1);
        if let Some(Some(focus)) = bubblepaw_runner::context_strategies::compact_command(text) {
            let summary_request = provider.requests.lock().unwrap().last().unwrap().clone();
            assert_fork_control_focus(&summary_request, focus);
        }
        let after = request(
            &client,
            &base,
            reqwest::Method::GET,
            &format!("/api/sessions/{session}"),
            json!({}),
        )
        .await;
        assert_initial_runtime_snapshot(&before["messages"], &after["messages"]);
    }
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}

/// docs/lcm-contract.md "Gather": the summary request forks the prepared main
/// conversation, keeps its opening system prompt and appends the summary control.
/// A manual focus belongs to that appended control, not to the preserved opening.
fn assert_fork_control_focus(request: &Value, focus: &str) {
    let messages = request["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "system", "{request}");
    let opening = messages[0]["content"].as_str().unwrap();
    assert!(opening.starts_with("Execution environment:"), "{request}");
    assert!(!opening.contains(focus), "{request}");
    let control = messages
        .iter()
        .skip(1)
        .find(|message| {
            message["role"] == "system"
                && message["content"]
                    .as_str()
                    .is_some_and(|content| content.starts_with("EPISODE MEMORY BRANCH"))
        })
        .unwrap_or_else(|| panic!("summary control missing: {request}"));
    assert!(
        control["content"].as_str().unwrap().contains(focus),
        "{request}"
    );
}

fn assert_initial_runtime_snapshot(before: &Value, after: &Value) {
    assert_eq!(
        after, before,
        "manual preparation only queues the runtime snapshot; dispatch flushes it"
    );
}
