//! Scripted transport checks; no provider network requests.
use serde_json::{Value, json};
use std::{collections::HashSet, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

#[cfg(feature = "web")]
#[path = "support/web_host.rs"]
mod web_host;

fn host(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bubblepaw"));
    command.args(["--set", "agent.execution_mode='js'"]);
    command
        .args([
            "--provider",
            "demo",
            "--no-auto-summarize",
            "--approve-all",
            "--workspace",
        ])
        .arg(root)
        .arg("--data-dir")
        .arg(root.join("state"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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
    settled: HashSet<String>,
}

impl StdioHost {
    fn start(root: &Path) -> Self {
        let mut child = host(root).arg("stdio").spawn().unwrap();
        Self {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
            next_id: 1,
            settled: HashSet::new(),
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
                assert!(self.output.read_line(&mut line).await.unwrap() > 0);
                let reply: Value = serde_json::from_str(&line).unwrap();
                if reply["params"]["type"] == "session_settled"
                    && let Some(session) = reply["params"]["session_id"].as_str()
                {
                    self.settled.insert(session.to_owned());
                }
                if reply["id"] == id {
                    assert!(reply.get("error").is_none(), "{reply}");
                    return reply["result"].clone();
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

    async fn turn(&mut self, session: &str, script: &str) -> Value {
        self.settled.remove(session);
        let started = self
            .rpc(
                "run.start",
                json!({"session_id":session,"text":"scripted fixture","script":script}),
            )
            .await;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let state = self.state(session).await;
                let run = state["runs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|run| run["id"] == started["id"])
                    .unwrap();
                if run["status"] != "running" && self.settled.contains(session) {
                    assert_eq!(run["status"], "completed", "{run}");
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
        drop(self.input);
        assert!(
            tokio::time::timeout(Duration::from_secs(10), self.child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
}

async fn fixture(root: &Path, rpc: &mut StdioHost) -> (String, String, Value, Value) {
    std::fs::write(root.join("note.txt"), "before\n").unwrap();
    let opened = rpc
        .rpc("session.open", json!({"title":"rewind fixture"}))
        .await;
    let session = opened["id"].as_str().unwrap().to_owned();
    let prefix = rpc
        .turn(
            &session,
            "await tools.todo({items:[{text:'before editing',status:'pending'}]}); return 'before editing';",
        )
        .await;
    let after = rpc
        .turn(
            &session,
            &format!(
                "await tools.todo({{items:[{{text:'after editing',status:'done'}}]}}); return await tools.edit({});",
                json!({"path":"note.txt","old_string":"before","new_string":"after"})
            ),
        )
        .await;
    assert_eq!(
        std::fs::read_to_string(root.join("note.txt")).unwrap(),
        "after\n"
    );
    // The only rewind target is the latest user message, the turn that edited the
    // file. Its fork keeps the history through that message and the todo state
    // from before the edit turn.
    let messages = after["messages"].as_array().unwrap();
    let index = messages
        .iter()
        .rposition(|message| message["role"] == "user")
        .unwrap();
    let cutoff = messages[index]["id"].as_str().unwrap().to_owned();
    let mut prefix = prefix;
    prefix["messages"] = json!(messages[..=index]);
    (session, cutoff, prefix, after)
}

fn assert_fork(result: &Value, source: &str, fork: &Value, prefix: &Value) {
    let id = result["session_id"].as_str().unwrap();
    assert_ne!(id, source);
    assert_eq!(fork["todo"], prefix["todo"]);
    assert!(fork["question"].is_null());
    // Stdio returns canonical metadata; web returns a smaller SessionSummary.
    if fork["session"].get("parent_session_id").is_some() {
        assert_eq!(fork["session"]["parent_session_id"], source);
        assert_eq!(
            fork["session"]["parent_cutoff"],
            prefix["messages"].as_array().unwrap().len()
        );
    }
    let copied = fork["messages"].as_array().unwrap();
    let original = prefix["messages"].as_array().unwrap();
    assert_eq!(copied.len(), original.len());
    for (copied, original) in copied.iter().zip(original) {
        assert_ne!(copied["id"], original["id"]);
        let mut expected = original.clone();
        expected["id"] = copied["id"].clone();
        expected["session_id"] = json!(id);
        assert_eq!(*copied, expected);
    }
    assert!(
        result["conflicts"].as_array().unwrap().is_empty(),
        "{result}"
    );
}

#[tokio::test]
async fn stdio_rewind_and_message_commands_restore_files_and_fork_without_new_turns() {
    for method in ["session.rewind", "run.start", "run.steer", "run.follow_up"] {
        let root = tempfile::tempdir().unwrap();
        let mut rpc = StdioHost::start(root.path());
        let initialized = rpc.rpc("initialize", json!({})).await;
        let features = initialized["features"].as_array().unwrap();
        for feature in ["todo", "question", "rewind"] {
            assert!(features.contains(&json!(feature)), "{initialized}");
        }
        let (session, cutoff, prefix, after) = fixture(root.path(), &mut rpc).await;
        let params = if method == "session.rewind" {
            json!({"session_id":session,"message_id":cutoff})
        } else {
            json!({"session_id":session,"text":format!("/rewind {cutoff}")})
        };
        let result = rpc.rpc(method, params).await;
        let fork = rpc.state(result["session_id"].as_str().unwrap()).await;
        assert_fork(&result, &session, &fork, &prefix);
        assert_eq!(
            std::fs::read_to_string(root.path().join("note.txt")).unwrap(),
            "before\n"
        );
        let source_after = rpc.state(&session).await;
        assert_eq!(source_after["messages"], after["messages"]);
        assert_eq!(source_after["runs"], after["runs"]);
        assert_eq!(source_after["inputs"], after["inputs"]);
        assert_eq!(source_after["todo"], after["todo"]);
        assert!(fork["runs"].as_array().unwrap().is_empty());
        rpc.shutdown().await;
    }
}

#[tokio::test]
async fn cli_rewind_restores_persisted_checkpoints_and_leaves_original_history_readable() {
    let root = tempfile::tempdir().unwrap();
    let mut rpc = StdioHost::start(root.path());
    let (session, cutoff, prefix, after) = fixture(root.path(), &mut rpc).await;
    rpc.shutdown().await;
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        host(root.path())
            .args(["run", &format!("/rewind {cutoff}"), "--session", &session])
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
    assert_eq!(
        std::fs::read_to_string(root.path().join("note.txt")).unwrap(),
        "before\n"
    );
    let mut rpc = StdioHost::start(root.path());
    let fork = rpc.state(result["session_id"].as_str().unwrap()).await;
    assert_fork(&result, &session, &fork, &prefix);
    assert_eq!(rpc.state(&session).await["messages"], after["messages"]);
    rpc.shutdown().await;
}

#[tokio::test]
async fn stdio_rewind_reports_conflicts_and_preserves_outside_file_edits() {
    let root = tempfile::tempdir().unwrap();
    let mut rpc = StdioHost::start(root.path());
    let (session, cutoff, prefix, after) = fixture(root.path(), &mut rpc).await;
    std::fs::write(root.path().join("note.txt"), "outside edit\n").unwrap();
    let result = rpc
        .rpc(
            "session.rewind",
            json!({"session_id":session,"message_id":cutoff}),
        )
        .await;
    assert_ne!(result["session_id"], session);
    assert!(result["restored"].as_array().unwrap().is_empty());
    assert_eq!(result["conflicts"].as_array().unwrap().len(), 1, "{result}");
    assert_eq!(result["conflicts"][0]["path"], "note.txt");
    assert_eq!(
        std::fs::read_to_string(root.path().join("note.txt")).unwrap(),
        "outside edit\n"
    );
    let fork = rpc.state(result["session_id"].as_str().unwrap()).await;
    assert_eq!(
        fork["messages"].as_array().unwrap().len(),
        prefix["messages"].as_array().unwrap().len()
    );
    assert_eq!(rpc.state(&session).await["messages"], after["messages"]);
    rpc.shutdown().await;
}

#[cfg(feature = "web")]
fn web_messages(messages: &Value) -> Value {
    let mut projected = messages.clone();
    for message in projected.as_array_mut().unwrap() {
        message.as_object_mut().unwrap().remove("provider_state");
    }
    projected
}

#[cfg(feature = "web")]
#[tokio::test]
async fn web_endpoint_and_message_commands_restore_the_same_persisted_boundary() {
    let root = tempfile::tempdir().unwrap();
    let mut rpc = StdioHost::start(root.path());
    let (session, cutoff, prefix, after) = fixture(root.path(), &mut rpc).await;
    let mut web_prefix = prefix.clone();
    assert!(
        prefix["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message.get("provider_state").is_some())
    );
    web_prefix["messages"] = web_messages(&prefix["messages"]);
    let web_after_messages = web_messages(&after["messages"]);
    rpc.shutdown().await;
    let assets = root.path().join("assets");
    std::fs::create_dir(&assets).unwrap();
    std::fs::write(assets.join("index.html"), "rewind fixture").unwrap();
    let (mut child, base) = web_host::start(host(root.path()), &assets).await;
    let client = reqwest::Client::new();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            assert!(child.try_wait().unwrap().is_none(), "web host exited");
            if client
                .get(format!("{base}/api/sessions/{session}"))
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
    let mut forks = Vec::new();
    for route in ["rewind", "run", "steer", "followup"] {
        std::fs::write(root.path().join("note.txt"), "after\n").unwrap();
        let params = if route == "rewind" {
            json!({"message_id":cutoff})
        } else {
            json!({"text":format!("/rewind {cutoff}")})
        };
        let response = client
            .post(format!("{base}/api/sessions/{session}/{route}"))
            .json(&params)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let result: Value = response.json().await.unwrap();
        assert!(status.is_success(), "{status} {result}");
        let fork: Value = client
            .get(format!(
                "{base}/api/sessions/{}",
                result["session_id"].as_str().unwrap()
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_fork(&result, &session, &fork, &web_prefix);
        forks.push(result["session_id"].as_str().unwrap().to_owned());
        assert_eq!(
            std::fs::read_to_string(root.path().join("note.txt")).unwrap(),
            "before\n"
        );
        let source: Value = client
            .get(format!("{base}/api/sessions/{session}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(source["messages"], web_after_messages);
    }
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    let store = bubblepaw_core::Store::open_data_dir(root.path().join("state")).unwrap();
    for id in forks {
        let fork = store.session(&id).unwrap();
        assert_eq!(fork.parent_session_id.as_deref(), Some(session.as_str()));
        assert_eq!(
            fork.parent_cutoff,
            Some(prefix["messages"].as_array().unwrap().len() as i64)
        );
        assert_fork(
            &json!({"session_id":id,"conflicts":[]}),
            &session,
            &json!({"session":fork,"messages":store.messages(&id).unwrap(),"todo":bubblepaw_runner::todo::list(&store, &id).unwrap()}),
            &prefix,
        );
    }
}
