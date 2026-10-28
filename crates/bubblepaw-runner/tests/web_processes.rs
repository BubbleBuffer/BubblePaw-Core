#![cfg(feature = "web")]
//! Real local HTTP coverage for owned command jobs: listing, user backgrounding and session ownership.
use bubblepaw_core::{CancellationToken, Harness, HarnessConfig, Store, ToolContext, ToolRegistry};
use bubblepaw_runner::{
    config::ExecConfig,
    provider::ScriptedProvider,
    tools::{ProcessTools, WorkspaceTools},
    web::{WebConfig, WebServices, serve_with_services},
};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

const SLEEP: &str = if cfg!(windows) {
    "ping -n 31 127.0.0.1 >nul"
} else {
    "sleep 30"
};

fn context(owner: &str) -> ToolContext {
    ToolContext {
        session_id: owner.into(),
        run_id: "run".into(),
        invocation_id: "invocation".into(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    base: String,
    client: Client,
    processes: Arc<ProcessTools>,
    registry: ToolRegistry,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<anyhow::Result<()>>,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let assets = directory.path().join("dist");
        std::fs::create_dir(&assets).unwrap();
        std::fs::write(assets.join("index.html"), "fixture").unwrap();
        let store = Store::open(directory.path().join("archive.sqlite")).unwrap();
        let harness = Arc::new(Harness::new(
            store.clone(),
            Arc::new(ScriptedProvider::new("fixture")),
            ToolRegistry::new(),
            HarnessConfig::default(),
        ));
        // The same ProcessTools instance backs the registry's exec tool and the HTTP routes.
        let processes = Arc::new(
            ProcessTools::with_config(
                WorkspaceTools::new(directory.path()).unwrap(),
                ExecConfig::default(),
                directory.path().join("data"),
            )
            .unwrap(),
        );
        let mut registry = ToolRegistry::new();
        processes.register(&mut registry).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let config = WebConfig {
            workspace: directory.path().display().to_string(),
            model: "fixture".into(),
            provider: "demo".into(),
            read_only: false,
        };
        let (shutdown, receive) = oneshot::channel();
        let task = tokio::spawn(serve_with_services(
            listener,
            harness,
            store,
            config,
            assets,
            WebServices {
                processes: Some(processes.clone()),
                ..Default::default()
            },
            async {
                let _ = receive.await;
            },
        ));
        Self {
            _directory: directory,
            base,
            client: Client::new(),
            processes,
            registry,
            shutdown,
            task,
        }
    }

    async fn get_json(&self, path: &str) -> (StatusCode, Value) {
        let response = self
            .client
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .unwrap();
        (response.status(), response.json().await.unwrap())
    }

    async fn post_background(&self, session: &str, handle: &str) -> (StatusCode, Value) {
        // Colons are percent-encoded the way the web client sends handles in paths.
        let encoded = handle.replace(':', "%3A");
        let response = self
            .client
            .post(format!(
                "{}/api/sessions/{session}/processes/{encoded}/background",
                self.base
            ))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        (response.status(), response.json().await.unwrap())
    }

    async fn create(&self) -> String {
        let response = self
            .client
            .post(format!("{}/api/sessions", self.base))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        let session: Value = response.json().await.unwrap();
        session["id"].as_str().unwrap().into()
    }

    // Polls the owner's listing over HTTP until its blocked foreground command appears.
    async fn foreground_handle(&self, session: &str) -> String {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let (status, listing) = self
                    .get_json(&format!("/api/sessions/{session}/processes"))
                    .await;
                assert_eq!(status, StatusCode::OK);
                let found = listing["jobs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|job| job["foreground"] == true)
                    .map(|job| job["handle"].as_str().unwrap().to_owned());
                if let Some(handle) = found {
                    return handle;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the blocked foreground command is listed for its owner")
    }
}

#[tokio::test]
async fn owner_backgrounds_its_blocked_command_and_other_sessions_cannot() {
    let fixture = Fixture::new().await;
    let owner = fixture.create().await;
    let other = fixture.create().await;
    let exec = fixture.registry.get("exec").unwrap();
    let owner_for_call = owner.clone();
    let call = tokio::spawn(async move {
        exec.invoke(context(&owner_for_call), json!({"command": SLEEP}))
            .await
    });
    let handle = fixture.foreground_handle(&owner).await;

    // Another session is told the handle belongs to someone else and nothing is released.
    let (status, refused) = fixture.post_background(&other, &handle).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("another conversation")
    );
    let (status, listing) = fixture
        .get_json(&format!("/api/sessions/{other}/processes"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listing, json!({"jobs": []}));

    // The owner's request runs while the exec call is still blocked in its sync wait.
    let (posted, reply) = tokio::join!(fixture.post_background(&owner, &handle), call);
    let (status, snapshot) = posted;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(snapshot["handle"], handle);
    assert_eq!(snapshot["status"], "backgrounded");
    assert_eq!(snapshot["reason"], "user");

    let result = reply.unwrap().unwrap();
    assert!(!result.is_error);
    let blocked: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(blocked["status"], "backgrounded");
    assert_eq!(blocked["reason"], "user");
    assert_eq!(blocked["handle"], handle);

    let (status, listing) = fixture
        .get_json(&format!("/api/sessions/{owner}/processes"))
        .await;
    assert_eq!(status, StatusCode::OK);
    let job = &listing["jobs"][0];
    assert_eq!(job["handle"], handle);
    assert_eq!(job["backgrounded"], true);
    assert_eq!(job["foreground"], false);

    // Cleanup: stop the owner's background job, then the server.
    fixture.processes.cancel_owner(&owner).await;
    fixture.processes.shutdown().await;
    fixture.shutdown.send(()).unwrap();
    fixture.task.await.unwrap().unwrap();
}
