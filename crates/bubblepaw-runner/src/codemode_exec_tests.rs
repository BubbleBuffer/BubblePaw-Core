use super::{CodeMode, View, WaitKind};
use crate::{
    artifacts::ArtifactStore,
    config::ExecConfig,
    net::{SearchBackend, WebConfig, register_fetch_search_tools},
    provider::{ScriptedProvider, ScriptedStep},
    tools::{ProcessTools, WorkspaceTools},
};
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    EventKind, Harness, HarnessConfig, Role, RunOptions, Store, Tool, ToolCall, ToolContext,
    ToolDefinition, ToolRegistry, ToolResult,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

#[test]
fn only_blocking_exec_calls_are_sync_waits() {
    for input in [
        json!({"command":"x"}),
        json!({"command":"x","yieldMs":25}),
        json!({"command":"x","background":false}),
    ] {
        assert_eq!(WaitKind::for_call("exec", &input), WaitKind::SyncExec);
    }
    for input in [
        json!({"command":"x","background":true}),
        json!({"command":"x","yieldMs":0}),
    ] {
        assert_eq!(WaitKind::for_call("exec", &input), WaitKind::None);
    }
    assert_eq!(
        WaitKind::for_call("wait", &json!({"ms":100})),
        WaitKind::Time
    );
    assert_eq!(
        WaitKind::for_call("wait", &json!({"handle":"agent:x"})),
        WaitKind::Operation
    );
    assert_eq!(
        WaitKind::for_call("poll", &json!({"handle":"process:x"})),
        WaitKind::None
    );
    for yield_ms in [json!(0), json!(-1), json!("100"), Value::Null] {
        assert_eq!(
            WaitKind::for_call("poll", &json!({"handle":"process:x", "yieldMs":yield_ms})),
            WaitKind::None
        );
    }
    for yield_ms in [1, 200, 30_000, 1_200_000] {
        assert_eq!(
            WaitKind::for_call("poll", &json!({"handle":"process:x", "yieldMs":yield_ms})),
            WaitKind::Operation
        );
    }
}

#[test]
fn exec_only_steer_preserves_cell_but_pending_ordinary_callbacks_and_queued_waits_detach() {
    let view = || View {
        execution_id: Some("cell".into()),
        waiting: 1,
        sync_exec_waiting: 1,
        ..Default::default()
    };
    let mut exec_only = view();
    assert!(!exec_only.background_on_steer());
    assert!(!exec_only.background);
    // A host-completed ordinary operation remains pending until its JS callback
    // resolves its promise, preserving the existing wait contract.
    let mut callback_pending = View {
        waiting: 2,
        ..view()
    };
    assert!(callback_pending.background_on_steer());
    callback_pending.waiting -= 1;
    assert!(callback_pending.background);
    assert!(!callback_pending.background_on_steer());
    let mut callback_consumed = view();
    assert!(!callback_consumed.background_on_steer());
    let mut queued_mixed = view();
    WaitKind::Time.enqueue(&mut queued_mixed);
    assert!(queued_mixed.background_on_steer());
    let mut queued_nonexec = View {
        execution_id: Some("cell".into()),
        ..Default::default()
    };
    WaitKind::Time.enqueue(&mut queued_nonexec);
    assert!(
        !queued_nonexec.background_on_steer(),
        "pure non-exec enrollment timing stays unchanged"
    );
    queued_nonexec.waiting = 1;
    assert!(queued_nonexec.background_on_steer());
}

struct StalledHost;
#[async_trait]
impl Tool for StalledHost {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "stall".into(),
            description: "Test ordinary host call.".into(),
            input_schema: json!({"type":"object","additionalProperties":false}),
            read_only: true,
            approval: Default::default(),
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        tokio::time::sleep(Duration::from_millis(250)).await;
        Ok(ToolResult::text("{}"))
    }
}

#[path = "codemode_exec_tests/support.rs"]
mod support;
use support::Fixture;

#[tokio::test]
async fn real_sync_exec_outlasts_both_shortened_idle_deadlines() {
    #[cfg(windows)]
    let command = "Start-Sleep -Milliseconds 200; [Console]::Out.Write('long-exec')";
    #[cfg(not(windows))]
    let command = "sleep 0.2; printf long-exec";
    let fixture = Fixture::new(format!("await tools.exec({})", json!({"command":command})));
    let result = fixture.result().await;
    assert!(!result.is_error, "{}", result.content);
    let result: Value =
        serde_json::from_str(result.content.split("\n[---]\n").next().unwrap()).unwrap();
    assert_eq!(result["status"], "exited");
    assert_eq!(result["exitCode"], 0);
    assert_eq!(result["stdout"], "long-exec");
    fixture.shutdown().await;
}

#[cfg(windows)]
const CONTROLLED_POLL_COMMAND: &str = "[Console]::Out.Write('ready'); while (-not (Test-Path 'release.txt')) { Start-Sleep -Milliseconds 20 }; [Console]::Out.Write('poll-finished')";
#[cfg(not(windows))]
const CONTROLLED_POLL_COMMAND: &str =
    "printf ready; while [ ! -e release.txt ]; do sleep 0.02; done; printf poll-finished";

#[tokio::test]
async fn real_yielding_poll_outlasts_idle_deadlines_and_preserves_cell_state_and_cursors() {
    let fixture = Fixture::new(format!(
        "state.marker='preserved'; const p=await tools.exec({start}); \
         const before=Date.now(); const first=await tools.poll({{handle:p.handle,yieldMs:200}}); \
         const elapsed=Date.now()-before; await wait({{handle:p.handle,timeoutMs:5000}}); \
         const final=await tools.poll({{handle:p.handle,cursor:{{stdout:0,stderr:0}}}}); \
         return {{marker:state.marker,firstStatus:first.status,elapsed,final}};",
        start = json!({"command":CONTROLLED_POLL_COMMAND,"yieldMs":1})
    ));
    let mut events = fixture.harness.subscribe();
    // The command cannot complete before the first poll's requested timeout:
    // release only when JS has advanced from that poll to its explicit wait.
    let release = async {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let EventKind::ToolStarted { call } = events.recv().await.unwrap().kind
                    && call.name == "wait"
                {
                    std::fs::write(fixture._root.path().join("release.txt"), "release").unwrap();
                    break;
                }
            }
        })
        .await
        .expect("yielding poll failed before reaching the completion wait");
    };
    let (result, ()) = tokio::join!(fixture.result(), release);
    assert!(!result.is_error, "{}", result.content);
    let result: Value =
        serde_json::from_str(result.content.split("\n[---]\n").next().unwrap()).unwrap();
    assert_eq!(result["marker"], "preserved");
    assert_eq!(result["firstStatus"], "running");
    assert!(result["elapsed"].as_u64().unwrap() >= 100, "{result}");
    assert_eq!(result["final"]["status"], "exited");
    assert_eq!(result["final"]["exitCode"], 0);
    assert_eq!(result["final"]["stdout"], "readypoll-finished");
    let environment = &fixture.mode.status(&fixture.session)["environments"][0];
    assert_eq!(environment["generation"], 1);
    assert_eq!(
        fixture.processes.jobs(&fixture.session).unwrap()["jobs"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn yielding_poll_keeps_cancellation_and_explicit_cell_background_controls() {
    for background in [false, true] {
        let fixture = Fixture::new(format!(
            "state.marker='waiting'; const p=await tools.exec({start}); \
             return await tools.poll({{handle:p.handle,yieldMs:1200000}});",
            start = json!({"command":CONTROLLED_POLL_COMMAND,"yieldMs":1})
        ));
        let mut events = fixture.harness.subscribe();
        fixture
            .harness
            .start(&fixture.session, "execute", RunOptions::default())
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let EventKind::ToolStarted { call } = events.recv().await.unwrap().kind
                    && call.name == "poll"
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while fixture.mode.status(&fixture.session)["environments"][0]["status"] != "waiting" {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the cell never reached waiting");
        let environment = &fixture.mode.status(&fixture.session)["environments"][0];
        assert_eq!(environment["generation"], 1);
        if background {
            assert_eq!(fixture.mode.background_waits(&fixture.session), 1);
            let run = tokio::time::timeout(
                Duration::from_secs(5),
                fixture.harness.wait_for_idle(&fixture.session),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(run.status, "completed");
            assert_eq!(
                fixture.mode.status(&fixture.session)["environments"][0]["status"],
                "background"
            );
            assert_eq!(
                fixture.processes.jobs(&fixture.session).unwrap()["jobs"][0]["status"],
                "running"
            );
        }
        assert!(fixture.harness.cancel(&fixture.session));
        if !background {
            let run = tokio::time::timeout(
                Duration::from_secs(5),
                fixture.harness.wait_for_idle(&fixture.session),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(run.status, "cancelled");
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = fixture.mode.status(&fixture.session);
                let environment = &snapshot["environments"][0];
                let jobs = fixture.processes.jobs(&fixture.session).unwrap();
                if environment["status"] == "idle" && jobs["jobs"][0]["status"] == "cancelled" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("waiting cell/process did not cancel");
        fixture.shutdown().await;
    }
}

#[tokio::test]
async fn ordinary_host_call_still_hits_the_shortened_deadline() {
    let fixture = Fixture::new("await tools.stall({})".into());
    let result = fixture.result().await;
    assert!(result.is_error, "{}", result.content);
    assert!(
        result.content.contains("Non-wait") && result.content.contains("deadline exceeded"),
        "{}",
        result.content
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn sync_exec_backgrounds_at_configured_timeout_after_outlasting_idle_limits() {
    #[cfg(windows)]
    let command = "[Console]::Out.Write('ready'); Start-Sleep -Seconds 30";
    #[cfg(not(windows))]
    let command = "printf ready; sleep 30";
    let fixture = Fixture::with_config(
        format!("await tools.exec({})", json!({"command":command})),
        ExecConfig {
            sync_timeout_secs: 1,
            ..Default::default()
        },
    );
    let result = fixture.result().await;
    assert!(!result.is_error, "{}", result.content);
    let result: Value =
        serde_json::from_str(result.content.split("\n[---]\n").next().unwrap()).unwrap();
    assert_eq!(result["status"], "backgrounded");
    assert_eq!(result["reason"], "sync_timeout");
    // Shell startup can outlast the foreground window. Output captured at
    // backgrounding may be empty or partial; readiness must remain observable.
    assert!("ready".starts_with(result["stdout"].as_str().unwrap()));
    assert!(result["handle"].as_str().unwrap().starts_with("process:"));
    let jobs = fixture.processes.jobs(&fixture.session).unwrap();
    assert_eq!(jobs["jobs"][0]["handle"], result["handle"]);
    assert_eq!(jobs["jobs"][0]["status"], "running");
    assert_eq!(jobs["jobs"][0]["backgrounded"], true);
    let mut registry = ToolRegistry::new();
    fixture.processes.register(&mut registry).unwrap();
    let poll = registry.get("poll").unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let polled = poll
                .invoke(
                    ToolContext {
                        session_id: fixture.session.clone(),
                        run_id: "readiness".into(),
                        invocation_id: "readiness-poll".into(),
                        cancellation: bubblepaw_core::CancellationToken::new(),
                        approval: Default::default(),
                    },
                    json!({"handle":result["handle"],"cursor":{"stdout":0,"stderr":0}}),
                )
                .await
                .unwrap();
            assert!(!polled.is_error, "{}", polled.content);
            let polled: Value = serde_json::from_str(&polled.content).unwrap();
            assert_eq!(polled["status"], "running");
            let stdout = polled["stdout"].as_str().unwrap();
            assert!("ready".starts_with(stdout), "{polled}");
            if stdout == "ready" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("backgrounded exec did not publish readiness output");
    fixture.shutdown().await;
}

struct PendingWebResponse {
    url: String,
    started: tokio::sync::oneshot::Receiver<()>,
    release: tokio::sync::oneshot::Sender<()>,
    closed: tokio::sync::oneshot::Receiver<()>,
    task: tokio::task::JoinHandle<()>,
}

async fn pending_web_response(search: bool) -> PendingWebResponse {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (started_tx, started) = tokio::sync::oneshot::channel();
    let (release, release_rx) = tokio::sync::oneshot::channel();
    let (closed_tx, closed) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0u8; 4096];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = socket.read(&mut buffer).await.unwrap();
            assert!(read > 0, "request ended before its headers");
            request.extend_from_slice(&buffer[..read]);
            assert!(request.len() <= 16 * 1024);
        }
        started_tx.send(()).unwrap();
        tokio::select! {
            _ = release_rx => {
                let body = if search {
                    r#"{"results":[{"title":"Delayed source","url":"https://example.com/source","content":"waited-search"}]}"#
                } else {
                    "waited-fetch"
                };
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", if search { "application/json" } else { "text/plain" }, body.len());
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            read = socket.read(&mut buffer) => {
                assert_eq!(read.unwrap(), 0, "cancelled GET connection should close");
                let _ = closed_tx.send(());
            }
        }
    });
    PendingWebResponse {
        url,
        started,
        release,
        closed,
        task,
    }
}

fn waiting_web_fixture(url: &str, search: bool) -> Fixture {
    let web = WebConfig {
        search: vec![SearchBackend::SearxNg { url: url.into() }],
        allow_private: true,
        cache_ttl_secs: 0,
        ..Default::default()
    };
    let code = if search {
        "await tools.web_search({query:'shortened-idle-deadline'})".into()
    } else {
        format!(
            "await tools.web_fetch({})",
            json!({"url":format!("{url}/page")})
        )
    };
    Fixture::with_config_and_web(code, ExecConfig::default(), Some(&web))
}

#[tokio::test]
async fn real_web_calls_outlast_shortened_idle_deadlines() {
    for search in [false, true] {
        let server = pending_web_response(search).await;
        let fixture = waiting_web_fixture(&server.url, search);
        let control = async {
            tokio::time::timeout(Duration::from_secs(2), server.started)
                .await
                .unwrap()
                .unwrap();
            // The request remains pending for five times the fixture's idle limit.
            tokio::time::sleep(Duration::from_millis(150)).await;
            server.release.send(()).unwrap();
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(fixture.result(), control)
        })
        .await
        .unwrap();
        assert!(!result.is_error, "{}", result.content);
        let value: Value =
            serde_json::from_str(result.content.split("\n[---]\n").next().unwrap()).unwrap();
        if search {
            assert_eq!(value["results"][0]["snippet"], "waited-search");
        } else {
            assert_eq!(value["content"], "waited-fetch");
        }
        server.task.await.unwrap();
        fixture.shutdown().await;
    }
}

#[tokio::test]
async fn real_web_calls_cancel_cleanly_after_outlasting_idle_deadlines() {
    for search in [false, true] {
        let server = pending_web_response(search).await;
        let fixture = waiting_web_fixture(&server.url, search);
        fixture
            .harness
            .start(&fixture.session, "execute", RunOptions::default())
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), server.started)
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            fixture.harness.cancel(&fixture.session),
            "web call must still be running"
        );
        let run = tokio::time::timeout(
            Duration::from_secs(2),
            fixture.harness.wait_for_idle(&fixture.session),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(run.status, "cancelled", "{:?}", run.error);
        tokio::time::timeout(Duration::from_secs(2), server.closed)
            .await
            .unwrap()
            .unwrap();
        drop(server.release);
        server.task.await.unwrap();
        fixture.shutdown().await;
    }
}

#[tokio::test]
async fn process_handle_wait_suspends_both_shortened_idle_deadlines() {
    #[cfg(windows)]
    let command = "Start-Sleep -Milliseconds 200";
    #[cfg(not(windows))]
    let command = "sleep 0.2";
    let fixture = Fixture::new(format!(
        "const p=await tools.exec({}); return await wait({{handle:p.handle}});",
        json!({"command":command,"background":true})
    ));
    let result = fixture.result().await;
    assert!(!result.is_error, "{}", result.content);
    let result: Value =
        serde_json::from_str(result.content.split("\n[---]\n").next().unwrap()).unwrap();
    assert_eq!(result["status"], "exited");
    assert_eq!(result["exitCode"], 0);
    fixture.shutdown().await;
}
