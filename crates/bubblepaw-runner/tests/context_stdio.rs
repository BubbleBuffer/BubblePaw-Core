//! Scripted context reads through the real stdio transport, without model calls.
#[path = "context_stdio/fixture.rs"]
mod fixture;

use bubblepaw_core::Store;
use fixture::Fixture;
use serde_json::{Value, json};
use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

struct Host {
    child: tokio::process::Child,
    input: tokio::process::ChildStdin,
    output: BufReader<tokio::process::ChildStdout>,
    notifications: Vec<Value>,
    diagnostics: Option<tokio::task::JoinHandle<String>>,
    next: u64,
}
impl Host {
    fn start(root: &std::path::Path) -> Self {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_bubblepaw"))
            .args(["--provider", "demo", "--no-auto-summarize", "--workspace"])
            .arg(root)
            .args([
                "--set",
                &format!("storage.data_dir={}", root.join("data").display()),
                "stdio",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let diagnostics = tokio::spawn(async move {
            let mut text = String::new();
            stderr
                .take(64 * 1024)
                .read_to_string(&mut text)
                .await
                .unwrap();
            for (name, value) in std::env::vars() {
                let name = name.to_ascii_uppercase();
                if value.len() >= 4
                    && ["KEY", "TOKEN", "SECRET", "PASSWORD", "COOKIE", "AUTH"]
                        .iter()
                        .any(|part| name.contains(part))
                {
                    text = text.replace(&value, "[redacted]");
                }
            }
            let fields = regex::Regex::new(
                r"(?i)(bearer\s+|(?:token|secret|password|api[_-]?key)\s*[:=]\s*)\S+",
            )
            .unwrap();
            fields
                .replace_all(&text, "$1[redacted]")
                .chars()
                .take(4_000)
                .collect()
        });
        Self {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
            notifications: Vec::new(),
            diagnostics: Some(diagnostics),
            next: 1,
        }
    }
    async fn call(&mut self, method: &str, params: Value) -> Value {
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
                if self.output.read_line(&mut line).await.unwrap() == 0 {
                    let status = self.child.try_wait().unwrap();
                    let task = self.diagnostics.take().unwrap();
                    let diagnostics = tokio::time::timeout(Duration::from_secs(2), task)
                        .await
                        .unwrap()
                        .unwrap();
                    panic!("stdio host closed stdout, status={status:?}, stderr={diagnostics}");
                }
                let value: Value = serde_json::from_str(&line).unwrap();
                if value["id"] == id {
                    return value;
                }
                self.notifications.push(value);
            }
        })
        .await
        .unwrap()
    }
    async fn rpc(&mut self, method: &str, params: Value) -> Value {
        let value = self.call(method, params).await;
        assert!(value.get("error").is_none(), "{method}: {value}");
        value["result"].clone()
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

fn assert_no_writes(before: &Value, after: &Value) {
    for key in [
        "transaction_count",
        "chat_transaction_count",
        "serving_transaction_count",
        "catalog_transaction_count",
        "message_count",
    ] {
        assert_eq!(before[key], after[key], "read changed {key}");
    }
}

#[tokio::test]
async fn stdio_context_reads_preserve_full_history_and_write_nothing() -> anyhow::Result<()> {
    let f = Fixture::new().await?;
    // A data directory has one owner. Release the fixture's in-process lease
    // before the stdio child becomes its host, retaining expected snapshots.
    f.store.flush_serving()?;
    drop(f.store);
    let mut host = Host::start(f.root.path());
    let initialized = host.rpc("initialize", json!({})).await;
    assert!(
        initialized["features"]
            .as_array()
            .unwrap()
            .iter()
            .any(|feature| feature == "context")
    );
    host.rpc("session.open", json!({"session_id":f.session}))
        .await;
    let before = host.rpc("storage.stats", json!({})).await;
    let view = host
        .rpc("context.view", json!({"session_id":f.session}))
        .await;
    assert_eq!(view["view"], serde_json::to_value(&f.view)?);
    let structure = host
        .rpc("context.structure", json!({"session_id":f.session}))
        .await;
    let episode = structure["episodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == f.episode.id)
        .unwrap();
    assert_eq!(episode["first_message_id"], f.episode.first_message_id);
    assert_eq!(episode["last_message_id"], f.episode.last_message_id);
    assert_eq!(episode["source_tokens"], f.episode.source_tokens);
    assert_eq!(episode["status"], "written");
    assert!(episode["summary_tokens"].as_u64().unwrap() > 0);
    assert!(episode.get("summary").is_none());
    let chapter = structure["chapters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == f.chapter.id)
        .unwrap();
    assert_eq!(chapter["status"], "open");
    assert_eq!(structure["summary_tokens_estimated"], true);
    assert!(chapter.get("summary").is_none());
    let summary = host
        .rpc(
            "context.summary",
            json!({"session_id":f.session,"kind":"episode","id":f.episode.id}),
        )
        .await;
    assert_eq!(summary["summary"], f.episode.summary);
    let open = host
        .rpc(
            "context.summary",
            json!({"session_id":f.session,"kind":"chapter","id":f.chapter.id}),
        )
        .await;
    assert!(open["summary"].is_null());
    let closed = structure["chapters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == f.closed_chapter.id)
        .unwrap();
    assert_eq!(closed["status"], "closed");
    assert_eq!(closed["first_message_id"], f.episode.first_message_id);
    assert_eq!(closed["last_message_id"], f.episode.last_message_id);
    assert_eq!(closed["source_tokens"], f.episode.source_tokens);
    assert!(closed.get("summary").is_none());
    let closed_body = host
        .rpc(
            "context.summary",
            json!({"session_id":f.session,"kind":"chapter","id":f.closed_chapter.id}),
        )
        .await;
    assert_eq!(
        closed_body["summary"],
        f.closed_chapter.summary.as_ref().unwrap().as_str()
    );
    let events = host
        .rpc("context.events", json!({"session_id":f.session}))
        .await;
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["kind"] == "applied" && event["trigger"] == "manual")
    );
    let durable = host
        .rpc("events.after", json!({"session_id":f.session,"after":0}))
        .await;
    assert!(
        durable["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["type"] == "context_compaction" && event["kind"] == "applied")
    );

    let range = host
        .rpc(
            "history.range",
            json!({"session_id":f.session,"first_seq":f.seq[0],"last_seq":f.seq[5],"limit":2}),
        )
        .await;
    assert_eq!(
        range["messages"][0]["message"],
        serde_json::to_value(&f.history[0])?
    );
    assert_eq!(range["messages"][0]["source"], "estimated");
    assert!(range["messages"][0]["tokens"].as_u64().unwrap() > 0);
    assert_eq!(
        range["messages"][1]["message"],
        serde_json::to_value(&f.history[1])?
    );
    assert_eq!(range["messages"][1]["source"], "reported");
    assert_eq!(range["messages"][1]["tokens"], 42);
    assert_eq!(range["next_seq"], f.seq[2]);
    let tail = host.rpc("history.range", json!({"session_id":f.session,"first_message_id":f.history[2].id,"last_message_id":f.history[5].id})).await;
    assert_eq!(tail["messages"].as_array().unwrap().len(), 4);
    assert_eq!(
        tail["messages"][1]["message"],
        serde_json::to_value(&f.history[3])?
    );
    assert_eq!(tail["messages"][1]["source"], "reported");
    assert_eq!(
        tail["messages"][1]["tokens"], 0,
        "reported zero is not an estimate"
    );
    assert!(tail["next_seq"].is_null());
    let after = host.rpc("storage.stats", json!({})).await;
    assert_no_writes(&before, &after);
    host.stop().await;
    let reopened = Store::open_data_dir(f.root.path().join("data"))?;
    assert_eq!(reopened.messages(&f.session)?, f.history);
    Ok(())
}

#[tokio::test]
async fn stdio_context_rejects_invalid_ranges_and_cross_session_summary_ids_without_writes()
-> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let other = f
        .store
        .create_session_with_workspace("Other", f.root.path())?
        .id;
    let foreign_workspace = tempfile::tempdir()?;
    let foreign = f
        .store
        .create_session_with_workspace("Foreign workspace", foreign_workspace.path())?
        .id;
    f.store.flush_serving()?;
    drop(f.store);
    let mut host = Host::start(f.root.path());
    host.rpc("initialize", json!({})).await;
    let before = host.rpc("storage.stats", json!({})).await;
    for params in [
        json!({"session_id":f.session,"first_seq":f.seq[0]}),
        json!({"session_id":f.session,"first_seq":f.seq[5],"last_seq":f.seq[0]}),
        json!({"session_id":f.session,"first_seq":-1,"last_seq":1}),
        json!({"session_id":f.session,"first_seq":0,"last_seq":10,"first_message_id":f.history[0].id,"last_message_id":f.history[5].id}),
        json!({"session_id":f.session,"first_message_id":f.history[0].id,"last_message_id":"missing"}),
        json!({"session_id":f.session,"first_seq":0,"last_seq":10,"limit":0}),
        json!({"session_id":f.session,"first_seq":0,"last_seq":10,"limit":1001}),
    ] {
        let reply = host.call("history.range", params).await;
        assert!(
            reply.get("error").is_some(),
            "invalid history range was accepted"
        );
    }
    let reply = host
        .call(
            "context.summary",
            json!({"session_id":other,"kind":"episode","id":f.episode.id}),
        )
        .await;
    assert!(reply.get("error").is_some());
    let reply = host
        .call(
            "context.summary",
            json!({"session_id":f.session,"kind":"unsupported","id":f.episode.id}),
        )
        .await;
    assert!(reply.get("error").is_some());
    for method in [
        "context.view",
        "context.structure",
        "context.summary",
        "context.events",
        "history.range",
    ] {
        let reply = host.call(method, json!({"session_id":foreign,"kind":"episode","id":f.episode.id,"first_seq":0,"last_seq":10})).await;
        assert!(
            reply.get("error").is_some(),
            "{method} bypassed workspace scope"
        );
    }
    assert_no_writes(&before, &host.rpc("storage.stats", json!({})).await);
    host.stop().await;
    Ok(())
}

#[tokio::test]
async fn shared_context_reads_preserve_all_store_transactions_and_bytes() -> anyhow::Result<()> {
    use bubblepaw_runner::context_reads;
    let f = Fixture::new().await?;
    let before = serde_json::to_value(f.store.storage_stats()?)?;
    let original_events = serde_json::to_value(f.store.context_events(&f.session)?)?;
    for (method, params) in [
        ("context.view", json!({})),
        ("context.structure", json!({})),
        (
            "context.summary",
            json!({"kind":"episode","id":f.episode.id}),
        ),
        (
            "context.summary",
            json!({"kind":"chapter","id":f.chapter.id}),
        ),
        ("context.events", json!({})),
        (
            "history.range",
            json!({"first_seq":f.seq[0],"last_seq":f.seq[5]}),
        ),
    ] {
        context_reads::read(&f.store, &f.session, method, &params)?;
        assert_no_writes(&before, &serde_json::to_value(f.store.storage_stats()?)?);
    }
    let tokens = context_reads::message_tokens(&f.store, &f.session, f.seq[0]..=f.seq[5])?;
    assert_eq!(tokens.len(), f.history.len());
    assert_eq!(
        tokens[0].source,
        bubblepaw_core::MessageTokenSource::Estimated
    );
    assert_eq!(
        tokens[1].source,
        bubblepaw_core::MessageTokenSource::Reported
    );
    assert_eq!(tokens[1].tokens, 42);
    assert_eq!(
        tokens[3].source,
        bubblepaw_core::MessageTokenSource::Reported
    );
    assert_eq!(tokens[3].tokens, 0);
    f.store.flush_serving()?;
    let after = serde_json::to_value(f.store.storage_stats()?)?;
    assert_no_writes(&before, &after);
    assert_eq!(before["database_bytes"], after["database_bytes"]);
    assert_eq!(before["wal_bytes"], after["wal_bytes"]);
    assert_eq!(f.store.messages(&f.session)?, f.history);
    assert_eq!(
        serde_json::to_value(f.store.context_view(&f.session)?.unwrap())?,
        serde_json::to_value(&f.view)?
    );
    assert_eq!(
        serde_json::to_value(f.store.context_events(&f.session)?)?,
        original_events
    );
    Ok(())
}
