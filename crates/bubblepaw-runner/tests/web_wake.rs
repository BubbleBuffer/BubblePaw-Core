#![cfg(feature = "web")]
//! Automatic host follow-ups over the real local HTTP/SSE API; in-process models only.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, EventKind, Harness, HarnessConfig, HarnessEvent, MessageOrigin,
    ModelRequest, ModelResponse, Provider, Role, Store, TextCallback, ToolContext, ToolRegistry,
};
use bubblepaw_runner::{
    agents::AgentTools,
    config::ExecConfig,
    tools::{ProcessTools, WorkspaceTools},
    wake::WakeCoordinator,
    web::{SessionView, WebConfig, WebServices, router_with_services},
};
use futures_util::{Stream, StreamExt};
use reqwest::Client;
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{Semaphore, oneshot},
    task::JoinHandle,
};

#[path = "web_wake/providers.rs"]
mod providers;
#[cfg(windows)]
#[path = "support/windows_command.rs"]
mod windows_command;
use providers::{ChildProvider, ParentProvider};

struct Fixture {
    directory: tempfile::TempDir,
    store: Store,
    parent: String,
    provider: Arc<ParentProvider>,
    child: Arc<ChildProvider>,
    agents: Arc<AgentTools>,
    processes: Arc<ProcessTools>,
    exec: Arc<dyn bubblepaw_core::Tool>,
    base: String,
    client: Client,
    shutdown: oneshot::Sender<()>,
    host: JoinHandle<std::io::Result<()>>,
    harness: Arc<Harness>,
}
impl Fixture {
    async fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let assets = directory.path().join("dist");
        std::fs::create_dir(&assets)?;
        std::fs::write(assets.join("index.html"), "synthetic fixture")?;
        let store = Store::open_data_dir(directory.path().join("data"))?;
        let provider = Arc::new(ParentProvider::default());
        let child = Arc::new(ChildProvider(Semaphore::new(0)));
        let agents = Arc::new(AgentTools::new(
            store.clone(),
            Harness::new(
                store.clone(),
                child.clone(),
                ToolRegistry::new(),
                HarnessConfig::default(),
            ),
            None,
        ));
        let processes = Arc::new(ProcessTools::with_config(
            WorkspaceTools::new(directory.path())?,
            ExecConfig::default(),
            directory.path().join("jobs-data"),
        )?);
        let mut tools = ToolRegistry::new();
        agents.register(&mut tools)?;
        processes.register(&mut tools)?;
        let exec = tools.get("exec").unwrap();
        let harness = Arc::new(Harness::new(
            store.clone(),
            provider.clone(),
            tools,
            HarnessConfig::default(),
        ));
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let router = router_with_services(
            harness.clone(),
            store.clone(),
            WebConfig {
                workspace: directory.path().display().to_string(),
                model: "scripted".into(),
                provider: "scripted".into(),
                read_only: true,
            },
            assets,
            listener.local_addr()?.port(),
            WebServices {
                agents: Some(agents.clone()),
                processes: Some(processes.clone()),
                ..Default::default()
            },
        )?;
        let (shutdown, stop) = oneshot::channel();
        let host = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = stop.await;
                })
                .await
        });
        // Roots created after the router starts must receive the same automatic fallback.
        let client = Client::new();
        let created: Value = client
            .post(format!("{base}/api/sessions"))
            .json(&json!({"title":"Automatic wake fixture"}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let parent = created["id"].as_str().unwrap().to_owned();
        Ok(Self {
            directory,
            store,
            parent,
            provider,
            child,
            agents,
            processes,
            exec,
            base,
            client,
            shutdown,
            host,
            harness,
        })
    }
    /// Requests the parent model received; wake barriers use other roots.
    fn parent_requests(&self) -> usize {
        let requests = self.provider.0.lock().unwrap();
        requests
            .iter()
            .filter(|request| request.session_id == self.parent)
            .count()
    }
    /// Waits until the host's wake driver has passed every notice published
    /// so far: it admits all due roots in one pass, so once a later required
    /// notice for a fresh root has a run, an earlier deliverable one would too.
    async fn wake_barrier(&self) -> Result<()> {
        let created: Value = self
            .client
            .post(format!("{}/api/sessions", self.base))
            .json(&json!({"title":"Wake barrier"}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let sentinel = created["id"].as_str().unwrap().to_owned();
        self.agents
            .wake_coordinator()
            .publish_required(&sentinel, "barrier", "barrier".into());
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.store.runs(&sentinel).unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        Ok(())
    }
    fn context(&self) -> ToolContext {
        ToolContext {
            session_id: self.parent.clone(),
            run_id: "synthetic-parent-launch".into(),
            invocation_id: "synthetic-host-call".into(),
            cancellation: CancellationToken::new(),
            approval: Default::default(),
        }
    }
    async fn post(&self, action: &str, value: Value) -> Result<Value> {
        Ok(self
            .client
            .post(format!(
                "{}/api/sessions/{}/{action}",
                self.base, self.parent
            ))
            .json(&value)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }
    async fn view(&self) -> Result<SessionView> {
        Ok(self
            .client
            .get(format!("{}/api/sessions/{}", self.base, self.parent))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }
    async fn events(
        &self,
    ) -> Result<impl Stream<Item = Result<impl AsRef<[u8]>, reqwest::Error>> + Unpin> {
        Ok(self
            .client
            .get(format!("{}/api/events", self.base))
            .send()
            .await?
            .error_for_status()?
            .bytes_stream())
    }
    async fn close(self) -> Result<()> {
        self.agents.shutdown().await?;
        self.processes.shutdown().await;
        self.shutdown.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), self.host).await???;
        self.harness.shutdown(Duration::from_secs(3)).await?;
        Ok(())
    }
}

async fn parent_run<S, B>(stream: &mut S, parent: &str) -> Result<Vec<HarnessEvent>>
where
    S: Stream<Item = Result<B, reqwest::Error>> + Unpin,
    B: AsRef<[u8]>,
{
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut buffer = String::new();
        let mut events = Vec::new();
        let mut run_id = None;
        loop {
            let chunk = stream.next().await.expect("SSE stays connected")?;
            buffer.push_str(std::str::from_utf8(chunk.as_ref())?);
            while let Some(end) = buffer.find("\n\n") {
                let record: String = buffer.drain(..end + 2).collect();
                if record.lines().any(|line| line == "event: reset") {
                    continue;
                }
                let Some(data) = record.lines().find_map(|line| line.strip_prefix("data: ")) else {
                    continue;
                };
                let event: HarnessEvent = serde_json::from_str(data)?;
                if event.session_id != parent {
                    continue;
                }
                if run_id.is_none() {
                    if !matches!(event.kind, EventKind::RunStarted { .. }) {
                        continue;
                    }
                    run_id = Some(event.run_id.clone());
                }
                if run_id.as_ref() != Some(&event.run_id) {
                    continue;
                }
                let finished = matches!(event.kind, EventKind::RunFinished { .. });
                events.push(event);
                if finished {
                    return Ok::<_, anyhow::Error>(events);
                }
            }
        }
    })
    .await?
}

fn assert_completed_stream(events: &[HarnessEvent], text: &str) {
    let started = events
        .iter()
        .position(|event| matches!(event.kind, EventKind::RunStarted { .. }))
        .expect("automatic RunStarted reaches SSE");
    let paragraph = events
        .iter()
        .position(
            |event| matches!(&event.kind, EventKind::Paragraph { text: value } if value == text),
        )
        .expect("automatic streamed paragraph reaches SSE");
    let completed = events.iter().position(|event| matches!(&event.kind, EventKind::RunFinished { status, .. } if status == "completed")).expect("automatic completed RunFinished reaches SSE");
    assert!(started < paragraph && paragraph < completed);
    assert!(
        events
            .iter()
            .all(|event| event.run_id == events[started].run_id)
    );
}

#[tokio::test]
async fn idle_child_completion_wakes_root_with_streamed_reply_and_durable_generated_notice()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let mut events = fixture.events().await?;
    fixture
        .post("run", json!({"text":"Explicit first request"}))
        .await?;
    assert_completed_stream(
        &parent_run(&mut events, &fixture.parent).await?,
        "Parent response 1.",
    );
    let child = fixture.agents.start(
        &fixture.context(),
        json!({"task":"Complete after the parent goes idle"}),
    )?;
    fixture.child.0.add_permits(1);
    assert_completed_stream(
        &parent_run(&mut events, &fixture.parent).await?,
        "Parent response 2.",
    );
    let view = fixture.view().await?;
    let notices: Vec<_> = view
        .messages
        .iter()
        .filter(|message| {
            message.role == Role::User
                && message.origin == Some(MessageOrigin::Generated)
                && message.content.starts_with("<harness ")
        })
        .collect();
    assert_eq!(notices.len(), 1);
    assert!(
        notices[0]
            .content
            .contains(child["sessionId"].as_str().unwrap())
    );
    {
        let requests = fixture.provider.0.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].messages.last().unwrap().content,
            notices[0].content
        );
    }
    assert_eq!(fixture.store.runs(&fixture.parent)?.len(), 2);
    drop(events);
    fixture.close().await
}

#[tokio::test]
async fn idle_background_exec_completion_uses_the_existing_sse_and_generated_notice_contract()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let mut events = fixture.events().await?;
    fixture
        .post("run", json!({"text":"Explicit first request"}))
        .await?;
    parent_run(&mut events, &fixture.parent).await?;
    #[cfg(windows)]
    let command = windows_command::powershell_file(
        fixture.directory.path(),
        "wait-release.ps1",
        "while (!(Test-Path release-exec)) { Start-Sleep -Milliseconds 10 }; [Console]::Out.Write('synthetic command output')",
    );
    #[cfg(not(windows))]
    let command =
        "while [ ! -e release-exec ]; do sleep 0.01; done; printf 'synthetic command output'";
    let result = fixture
        .exec
        .invoke(
            fixture.context(),
            json!({"command":command,"background":true}),
        )
        .await?;
    assert!(!result.is_error, "{}", result.content);
    let job: Value = serde_json::from_str(&result.content)?;
    std::fs::write(fixture.directory.path().join("release-exec"), "release")?;
    assert_completed_stream(
        &parent_run(&mut events, &fixture.parent).await?,
        "Parent response 2.",
    );
    let view = fixture.view().await?;
    let notices: Vec<_> = view
        .messages
        .iter()
        .filter(|message| {
            message.origin == Some(MessageOrigin::Generated)
                && message.content.starts_with("<harness ")
        })
        .collect();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].content.contains(job["handle"].as_str().unwrap()));
    assert!(notices[0].content.contains("exit code 0"));
    assert_eq!(fixture.provider.0.lock().unwrap().len(), 2);
    drop(events);
    fixture.close().await
}

#[tokio::test]
async fn web_stop_suppresses_idle_completion_until_a_new_typed_request_is_accepted() -> Result<()> {
    let fixture = Fixture::new().await?;
    let mut events = fixture.events().await?;
    fixture
        .post("run", json!({"text":"Explicit first request"}))
        .await?;
    parent_run(&mut events, &fixture.parent).await?;
    fixture.post("cancel", json!({})).await?;
    let child = fixture
        .agents
        .start(&fixture.context(), json!({"task":"Completion after Stop"}))?;
    fixture.child.0.add_permits(1);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if fixture
                .store
                .runs(child["sessionId"].as_str().unwrap())
                .unwrap()
                .last()
                .is_some_and(|run| run.status == "completed")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    fixture.wake_barrier().await?;
    assert_eq!(
        fixture.parent_requests(),
        1,
        "Stop must remain sticky while the parent is idle"
    );
    fixture
        .post("run", json!({"text":"Explicitly continue"}))
        .await?;
    parent_run(&mut events, &fixture.parent).await?;
    fixture.agents.start(
        &fixture.context(),
        json!({"task":"Completion after explicit input"}),
    )?;
    fixture.child.0.add_permits(1);
    assert_completed_stream(
        &parent_run(&mut events, &fixture.parent).await?,
        "Parent response 3.",
    );
    assert_eq!(fixture.parent_requests(), 3);
    drop(events);
    fixture.close().await
}

#[tokio::test]
async fn router_rejects_process_services_bound_to_a_different_completion_inbox() -> Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("index.html"), "synthetic fixture")?;
    let store = Store::in_memory()?;
    let provider = Arc::new(ParentProvider::default());
    let harness = Arc::new(Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig::default(),
    ));
    let agents = Arc::new(AgentTools::new(
        store.clone(),
        Harness::new(
            store.clone(),
            provider,
            ToolRegistry::new(),
            HarnessConfig::default(),
        ),
        None,
    ));
    let processes = Arc::new(ProcessTools::new(WorkspaceTools::new(directory.path())?));
    processes.set_wake(Arc::new(WakeCoordinator::default()));
    let error = router_with_services(
        harness,
        store,
        WebConfig {
            workspace: "fixture".into(),
            model: "scripted".into(),
            provider: "scripted".into(),
            read_only: true,
        },
        directory.path().to_owned(),
        8788,
        WebServices {
            agents: Some(agents),
            processes: Some(processes),
            ..Default::default()
        },
    )
    .expect_err("mismatched services must fail before the host starts");
    assert!(
        error
            .to_string()
            .contains("must share the wake coordinator")
    );
    Ok(())
}
