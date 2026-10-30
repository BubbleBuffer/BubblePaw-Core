//! Real host processes and loopback providers only. Credentials are synthetic.
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
    process::{ChildStdin, ChildStdout, Command},
    sync::Notify,
    task::JoinHandle,
};

#[cfg(feature = "web")]
#[path = "support/web_host.rs"]
mod web_host;

struct Fixture {
    base: String,
    seen: Arc<Mutex<Vec<Value>>>,
    authorized: Arc<Mutex<Vec<bool>>>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    task: JoinHandle<()>,
}
impl Fixture {
    async fn new(block_tool_round: bool, expected_auth: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let authorized = Arc::new(Mutex::new(Vec::new()));
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let app = Router::new().route("/chat/completions", post({
            let seen = seen.clone(); let entered = entered.clone(); let release = release.clone();
            let authorized = authorized.clone();
            move |headers: axum::http::HeaderMap, Json(body): Json<Value>| {
                let seen = seen.clone(); let entered = entered.clone(); let release = release.clone();
                let authorized = authorized.clone();
                async move {
                    authorized.lock().unwrap().push(headers.get("authorization")
                        .and_then(|header| header.to_str().ok()) == Some(expected_auth));
                    seen.lock().unwrap().push(body);
                    entered.notify_one();
                    let message = if block_tool_round {
                        release.notified().await;
                        json!({"role":"assistant","content":null,"reasoning_content":"old-origin-private-reasoning",
                            "tool_calls":[{"id":"round-tool","type":"function","function":{"name":"js","arguments":"{\"code\":\"return 1;\"}"}}]})
                    } else { json!({"role":"assistant","content":"fixture finished"}) };
                    Json(json!({"choices":[{"message":message,"finish_reason":if block_tool_round {"tool_calls"} else {"stop"}}],
                        "usage":{"prompt_tokens":10,"completion_tokens":2}}))
                }
            }
        }));
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base,
            seen,
            authorized,
            entered,
            release,
            task,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn config(main: &str, provider: &str, base: &str, model: &str) -> String {
    format!(
        "[agent]\nexecution_mode='js'\n[model]\nmain='{main}'\n[providers.{provider}]\nkind='chat-completions'\nbase_url='{base}'\nkey_env='BP_RELOAD_FIXTURE_KEY'\nmodels=[{{id='{model}'}}]\n"
    )
}
fn host(root: &Path, file: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_bubblepaw"));
    cmd.arg("--config")
        .arg(file)
        .arg("--workspace")
        .arg(root)
        .arg("--data-dir")
        .arg(root.join("state"))
        .args(["--no-auto-summarize", "--read-only"])
        .env("BP_RELOAD_FIXTURE_KEY", "synthetic-local-fixture")
        .env("BP_RELOAD_FIXTURE_ALT_TOKEN", "synthetic-alternate-fixture")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // Prevent the developer's provider overrides from changing these fixtures.
    for (name, _) in std::env::vars() {
        if name.starts_with("BUBBLEPAW_") {
            cmd.env_remove(name);
        }
    }
    cmd
}
struct RpcOutput {
    reader: BufReader<ChildStdout>,
    settled: std::collections::HashSet<String>,
}

async fn rpc(
    input: &mut ChildStdin,
    output: &mut RpcOutput,
    id: u64,
    method: &str,
    params: Value,
) -> Value {
    if method == "run.start"
        && let Some(session) = params["session_id"].as_str()
    {
        output.settled.remove(session);
    }
    input
        .write_all(
            format!(
                "{}\n",
                json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    input.flush().await.unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let mut line = String::new();
            assert!(
                output.reader.read_line(&mut line).await.unwrap() > 0,
                "host exited before RPC reply"
            );
            let record: Value = serde_json::from_str(&line).unwrap();
            if record["params"]["type"] == "session_settled"
                && let Some(session) = record["params"]["session_id"].as_str()
            {
                output.settled.insert(session.into());
            }
            if record["id"] == id {
                return record;
            }
        }
    })
    .await
    .unwrap()
}
fn latest_run(state: &Value) -> &Value {
    state["runs"].as_array().unwrap().last().unwrap()
}
async fn state_finished(input: &mut ChildStdin, output: &mut RpcOutput, id: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let state = rpc(input, output, 99, "session.state", json!({"session_id":id})).await;
            let latest = state["result"]["runs"]
                .as_array()
                .and_then(|runs| runs.last());
            if latest.is_some_and(|run| run["status"] != "running") && output.settled.contains(id) {
                return state["result"].clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn stdio_reload_changes_the_next_tool_round_and_rejects_removed_pin() {
    let old = Fixture::new(true, "Bearer synthetic-local-fixture").await;
    let new = Fixture::new(false, "Bearer synthetic-alternate-fixture").await;
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("config.toml");
    std::fs::write(&file, config("old/first", "old", &old.base, "first")).unwrap();
    let mut child = host(root.path(), &file).arg("stdio").spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = RpcOutput {
        reader: BufReader::new(child.stdout.take().unwrap()),
        settled: Default::default(),
    };
    let opened = rpc(&mut input, &mut output, 1, "session.open", json!({})).await;
    let session = opened["result"]["id"].as_str().unwrap();
    let started = rpc(
        &mut input,
        &mut output,
        2,
        "run.start",
        json!({"session_id":session,"text":"two rounds"}),
    )
    .await;
    assert!(started.get("error").is_none());
    tokio::time::timeout(Duration::from_secs(10), old.entered.notified())
        .await
        .unwrap();
    std::fs::write(
        &file,
        config("new/second", "new", &new.base, "second")
            .replace("BP_RELOAD_FIXTURE_KEY", "BP_RELOAD_FIXTURE_ALT_TOKEN"),
    )
    .unwrap();
    let applied = rpc(&mut input, &mut output, 3, "config.reload", json!({})).await;
    assert_eq!(applied["result"]["active_main"], "new/second");
    old.release.notify_one();
    let finished = state_finished(&mut input, &mut output, session).await;
    assert_eq!(latest_run(&finished)["status"], "completed");
    assert_eq!(old.seen.lock().unwrap()[0]["model"], "first");
    {
        let requests = new.seen.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["model"], "second");
        assert!(
            requests[0]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|message| message.get("reasoning_content").is_none())
        );
    }
    // Failed reload leaves the published generation serving.
    std::fs::write(&file, "[model]\nmain='new/second'\n[providers.new]\nkind='chat-completions'\nbase_url='http://example.com'\nkey_env='BP_RELOAD_FIXTURE_KEY'\n").unwrap();
    let rejected = rpc(&mut input, &mut output, 4, "config.reload", json!({})).await;
    assert_eq!(rejected["result"]["applied"], false);
    assert!(
        rejected["result"]["errors"][0]
            .as_str()
            .unwrap()
            .contains("https")
    );
    rpc(
        &mut input,
        &mut output,
        5,
        "run.start",
        json!({"session_id":session,"text":"still new"}),
    )
    .await;
    assert_eq!(
        latest_run(&state_finished(&mut input, &mut output, session).await)["status"],
        "completed"
    );
    assert_eq!(new.seen.lock().unwrap().len(), 2);
    // An explicit pin never silently follows the new default.
    rpc(
        &mut input,
        &mut output,
        6,
        "run.start",
        json!({"session_id":session,"text":"removed pin","options":{"model":"old/first"}}),
    )
    .await;
    let failed = state_finished(&mut input, &mut output, session).await;
    assert_eq!(latest_run(&failed)["status"], "failed");
    assert!(
        latest_run(&failed)["error"]
            .as_str()
            .unwrap()
            .contains("old")
    );
    assert_eq!(new.seen.lock().unwrap().len(), 2);
    assert!(
        old.authorized
            .lock()
            .unwrap()
            .iter()
            .all(|accepted| *accepted)
    );
    assert!(
        new.authorized
            .lock()
            .unwrap()
            .iter()
            .all(|accepted| *accepted)
    );
    rpc(&mut input, &mut output, 7, "shutdown", json!({})).await;
    assert!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

#[cfg(feature = "web")]
async fn web_ready(child: &mut tokio::process::Child, base: &str) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            assert!(
                child.try_wait().unwrap().is_none(),
                "web host exited at startup"
            );
            if reqwest::get(format!("{base}/api/providers"))
                .await
                .is_ok_and(|reply| reply.status().is_success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}
#[cfg(feature = "web")]
async fn web_request(
    client: &reqwest::Client,
    base: &str,
    method: reqwest::Method,
    path: &str,
    body: Value,
) -> Value {
    let reply = client
        .request(method, format!("{base}{path}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(
        reply.status().is_success(),
        "host status {}",
        reply.status()
    );
    reply.json().await.unwrap()
}
#[cfg(feature = "web")]
async fn web_turn(client: &reqwest::Client, base: &str, session: &str) {
    let started = web_request(
        client,
        base,
        reqwest::Method::POST,
        &format!("/api/sessions/{session}/run"),
        json!({"text":"next"}),
    )
    .await;
    let run = started["id"].as_str().unwrap();
    // The session view refreshes after the reply, so it may not list this run yet.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let state: Value = client
                .get(format!("{base}/api/sessions/{session}"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let listed = state["runs"]
                .as_array()
                .and_then(|runs| runs.iter().find(|listed| listed["id"] == run));
            if let Some(listed) = listed
                && listed["status"] != "running"
                && state["active_run_id"].is_null()
            {
                assert_eq!(listed["status"], "completed");
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[cfg(feature = "web")]
#[tokio::test]
async fn web_setup_save_and_disk_reload_apply_to_the_running_host_atomically() {
    let first = Fixture::new(false, "Bearer synthetic-local-fixture").await;
    let second = Fixture::new(false, "Bearer synthetic-alternate-fixture").await;
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("config.toml");
    std::fs::write(
        &file,
        config("custom/first", "custom", &first.base, "first"),
    )
    .unwrap();
    let (mut child, base) = web_host::start(host(root.path(), &file)).await;
    web_ready(&mut child, &base).await;
    let client = reqwest::Client::new();
    let opened = web_request(
        &client,
        &base,
        reqwest::Method::POST,
        "/api/sessions",
        json!({}),
    )
    .await;
    let session = opened["id"].as_str().unwrap();
    web_turn(&client, &base, session).await;
    let saved = web_request(&client, &base, reqwest::Method::PUT, "/api/providers/custom", json!({
        "kind":"chat-completions","base_url":second.base,"key_env":"BP_RELOAD_FIXTURE_ALT_TOKEN","models":[{"id":"first"},{"id":"second"}]})).await;
    assert_eq!(saved["reload_needed"], false);
    assert_eq!(saved["restart_needed"], false);
    web_turn(&client, &base, session).await;
    assert_eq!(second.seen.lock().unwrap()[0]["model"], "first");
    let selected = web_request(
        &client,
        &base,
        reqwest::Method::PUT,
        "/api/roles/main",
        json!({"model":"custom/second"}),
    )
    .await;
    assert_eq!(selected["active_main"], "custom/second");
    let current = web_request(
        &client,
        &base,
        reqwest::Method::GET,
        "/api/config",
        json!({}),
    )
    .await;
    assert_eq!(current["model"], "custom/second");
    assert_eq!(current["provider"], "custom");
    web_turn(&client, &base, session).await;
    assert_eq!(second.seen.lock().unwrap()[1]["model"], "second");
    let before = std::fs::read_to_string(&file).unwrap();
    let invalid = client.put(format!("{base}/api/providers/custom")).json(&json!({
        "kind":"chat-completions","base_url":"http://example.com","key_env":"BP_RELOAD_FIXTURE_KEY","models":[{"id":"second"}]})).send().await.unwrap();
    assert_eq!(invalid.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    web_turn(&client, &base, session).await;
    assert_eq!(second.seen.lock().unwrap().len(), 3);
    std::fs::write(&file, config("disk/third", "disk", &first.base, "third")).unwrap();
    let pending = web_request(
        &client,
        &base,
        reqwest::Method::GET,
        "/api/providers",
        json!({}),
    )
    .await;
    assert_eq!(pending["reload_needed"], true);
    assert_eq!(pending["active_main"], "custom/second");
    let reloaded = web_request(
        &client,
        &base,
        reqwest::Method::POST,
        "/api/config/reload",
        json!({}),
    )
    .await;
    assert_eq!(reloaded["active_main"], "disk/third");
    assert_eq!(reloaded["reload_needed"], false);
    web_turn(&client, &base, session).await;
    assert_eq!(first.seen.lock().unwrap()[1]["model"], "third");
    // The new endpoint stays behind the same local boundary as setup writes.
    let foreign = client
        .post(format!("{base}/api/config/reload"))
        .header("Origin", "https://foreign.invalid")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(foreign.status(), reqwest::StatusCode::FORBIDDEN);
    assert!(
        first
            .authorized
            .lock()
            .unwrap()
            .iter()
            .all(|accepted| *accepted)
    );
    assert!(
        second
            .authorized
            .lock()
            .unwrap()
            .iter()
            .all(|accepted| *accepted)
    );
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}
