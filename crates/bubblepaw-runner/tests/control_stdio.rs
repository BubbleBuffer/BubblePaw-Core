//! Scripted end-to-end acceptance of the control tools and client protocol.
use serde_json::{Value, json};
use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct Host {
    child: tokio::process::Child,
    input: tokio::process::ChildStdin,
    output: BufReader<tokio::process::ChildStdout>,
    events: Vec<Value>,
    next: u64,
}
impl Host {
    fn start(root: &std::path::Path) -> Self {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_bubblepaw"))
            .args(["--set", "agent.execution_mode='js'"])
            .args(["--provider", "demo", "--no-auto-summarize", "--workspace"])
            .arg(root)
            .args([
                "--set",
                &format!("storage.data_dir={}", root.join("data").display()),
                "stdio",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        Self {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
            events: Vec::new(),
            next: 1,
        }
    }
    async fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        let line = format!(
            "{}\n",
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        );
        self.input.write_all(line.as_bytes()).await.unwrap();
        self.input.flush().await.unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let mut line = String::new();
                assert!(
                    self.output.read_line(&mut line).await.unwrap() > 0,
                    "host closed stdout"
                );
                let value: Value = serde_json::from_str(&line).unwrap();
                if value["id"] == id {
                    assert!(value.get("error").is_none(), "{method}: {value}");
                    return value["result"].clone();
                }
                self.events.push(value);
            }
        })
        .await
        .unwrap()
    }
    async fn state_until(&mut self, session: &str, predicate: impl Fn(&Value) -> bool) -> Value {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let state = self
                    .rpc("session.state", json!({"session_id":session}))
                    .await;
                if predicate(&state) {
                    return state;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap()
    }
    async fn stop(mut self) {
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

#[tokio::test]
async fn stdio_todo_root_answer_and_cancel() {
    let root = tempfile::tempdir().unwrap();
    let mut host = Host::start(root.path());
    let session = host
        .rpc("session.open", json!({"title":"control tools"}))
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    host.rpc("run.start", json!({"session_id":session,"text":"ask fixture","script":"await tools.todo({items:[{text:'check answer',status:'in_progress'}]}); return await tools.question({text:'Which colour?',options:['Blue','Green']});"})).await;
    let pending = host
        .state_until(&session, |state| state["question"]["status"] == "pending")
        .await;
    assert_eq!(pending["todo"][0]["text"], "check answer");
    assert_eq!(pending["question"]["text"], "Which colour?");
    host.rpc(
        "question.answer",
        json!({"session_id":session,"question_id":pending["question"]["id"],"text":"Green"}),
    )
    .await;
    let completed = host
        .state_until(&session, |state| {
            state["runs"]
                .as_array()
                .is_some_and(|runs| runs.iter().all(|run| run["status"] == "completed"))
        })
        .await;
    assert!(completed["question"].is_null());
    assert_eq!(
        completed["runs"].as_array().unwrap().len(),
        1,
        "answer receipts must not start a model after a script"
    );
    assert_eq!(completed["runs"][0]["model"], "script");
    assert!(
        completed["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["content"]
                .as_str()
                .is_some_and(|text| text.contains("Green")))
    );
    let events = host
        .rpc("events.after", json!({"session_id":session,"after":0}))
        .await;
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["type"] == "host_state_changed" && event["key"] == "todo")
    );
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["type"] == "host_state_changed"
                && event["key"] == "question"
                && event["value"]["status"] == "pending")
    );
    host.rpc("run.start", json!({"session_id":session,"text":"cancel fixture","script":"return await tools.question({text:'Wait here'});"})).await;
    host.state_until(&session, |state| state["question"]["status"] == "pending")
        .await;
    assert_eq!(
        host.rpc("run.cancel", json!({"session_id":session})).await["requested"],
        true
    );
    let cancelled = host
        .state_until(&session, |state| {
            state["runs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|run| run["status"] == "cancelled")
        })
        .await;
    assert!(cancelled["question"].is_null());
    host.stop().await;
}

#[tokio::test]
async fn unattended_script_question_returns_immediately() {
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("question.js");
    std::fs::write(
        &script,
        "return await tools.question({text:'Anyone available?'});",
    )
    .unwrap();
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_bubblepaw"))
            .args(["--set", "agent.execution_mode='js'"])
            .args([
                "--provider",
                "demo",
                "--no-auto-summarize",
                "--json",
                "--workspace",
            ])
            .arg(root.path())
            .args([
                "--set",
                &format!("storage.data_dir={}", root.path().join("data").display()),
                "script",
            ])
            .arg(script)
            .kill_on_drop(true)
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
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("no_user_available"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}
