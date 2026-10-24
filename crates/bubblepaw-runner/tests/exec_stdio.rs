//! Scripted CLI transport acceptance; no live model calls.
use serde_json::{Value, json};
use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

async fn rpc(
    input: &mut tokio::process::ChildStdin,
    output: &mut BufReader<tokio::process::ChildStdout>,
    id: u64,
    method: &str,
    params: Value,
) -> Value {
    let request = format!(
        "{}\n",
        json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
    );
    input.write_all(request.as_bytes()).await.unwrap();
    input.flush().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut line = String::new();
            assert!(
                output.read_line(&mut line).await.unwrap() > 0,
                "runner closed stdout"
            );
            let reply: Value = serde_json::from_str(&line).unwrap();
            if reply["id"] == id {
                return reply;
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn stdio_background_releases_sync_exec_and_cancel_removes_spool() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_bubblepaw"))
        .args(["--set", "agent.execution_mode='js'"])
        .args(["--provider", "demo", "--no-auto-summarize", "--workspace"])
        .arg(root.path())
        .args([
            "--set",
            &format!("storage.data_dir={}", data.display()),
            "--set",
            "exec.max_output_bytes=64",
            "stdio",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let opened = rpc(
        &mut input,
        &mut output,
        1,
        "session.open",
        json!({"title":"exec stdio"}),
    )
    .await;
    assert!(opened.get("error").is_none(), "{opened}");
    let session = opened["result"]["id"].as_str().unwrap().to_owned();
    #[cfg(unix)]
    let command = "printf ready; sleep 30";
    #[cfg(windows)]
    let command = "[Console]::Out.Write('ready'); Start-Sleep -Seconds 30";
    let script = format!("return await tools.exec({});", json!({"command":command}));
    let started = rpc(
        &mut input,
        &mut output,
        2,
        "run.start",
        json!({"session_id":session,"text":"scripted command","script":script}),
    )
    .await;
    assert!(started.get("error").is_none(), "{started}");
    let mut id = 3;
    let handle = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let listed = rpc(
                &mut input,
                &mut output,
                id,
                "process.list",
                json!({"session_id":session}),
            )
            .await;
            id += 1;
            assert!(listed.get("error").is_none(), "{listed}");
            if let Some(job) = listed["result"]["jobs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|job| job["foreground"] == true)
            {
                return job["handle"].as_str().unwrap().to_owned();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let background = rpc(
        &mut input,
        &mut output,
        id,
        "process.background",
        json!({"session_id":session,"handle":handle}),
    )
    .await;
    id += 1;
    assert_eq!(
        background["result"]["status"], "backgrounded",
        "{background}"
    );
    assert_eq!(background["result"]["reason"], "user");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let state = rpc(
                &mut input,
                &mut output,
                id,
                "session.state",
                json!({"session_id":session}),
            )
            .await;
            id += 1;
            if state["result"]["runs"][0]["status"] == "completed" {
                assert!(
                    state["result"]["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(
                            |message| message["content"].as_str().is_some_and(|text| text
                                .contains("backgrounded")
                                && text.contains("user"))
                        )
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let cancelled = rpc(
        &mut input,
        &mut output,
        id,
        "run.cancel",
        json!({"session_id":session}),
    )
    .await;
    id += 1;
    assert_eq!(cancelled["result"]["requested"], true);
    let files = std::fs::read_dir(data.join("jobs").join(&session))
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(files, 0, "cancel removes spool immediately");
    let stopped = rpc(&mut input, &mut output, id, "shutdown", json!({})).await;
    assert!(stopped.get("error").is_none(), "{stopped}");
    drop(input);
    assert!(
        tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}
