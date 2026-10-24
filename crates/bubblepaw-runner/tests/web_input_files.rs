#![cfg(feature = "web")]
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, Store,
    TextCallback, ToolRegistry,
};
use bubblepaw_runner::{
    input_files::{InputFiles, InputLimits},
    provider::ScriptedProvider,
    web::{WebConfig, WebServices, serve_with_services},
};
use reqwest::{Client, StatusCode};
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

struct Capture {
    requests: Mutex<Vec<ModelRequest>>,
    entered: Semaphore,
    release: Semaphore,
    scripted: ScriptedProvider,
}
impl Capture {
    fn new(gated: bool) -> Self {
        Self {
            requests: Mutex::default(),
            entered: Semaphore::new(0),
            release: Semaphore::new(if gated { 0 } else { 100 }),
            scripted: ScriptedProvider::new("Scripted attachment verification complete."),
        }
    }
}
#[async_trait]
impl Provider for Capture {
    async fn generate(
        &self,
        request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request.clone());
        self.entered.add_permits(1);
        tokio::select! {
            permit = self.release.acquire() => { permit?.forget(); },
            _ = cancellation.cancelled() => anyhow::bail!("cancelled"),
        }
        self.scripted.generate(request, on_text, cancellation).await
    }
}
struct Server {
    directory: tempfile::TempDir,
    base: String,
    client: Client,
    harness: Arc<Harness>,
    provider: Arc<Capture>,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<Result<()>>,
}
impl Server {
    async fn new(limits: Option<InputLimits>, gated: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let assets = directory.path().join("dist");
        std::fs::create_dir(&assets).unwrap();
        std::fs::write(assets.join("index.html"), "fixture").unwrap();
        let store = Store::open(directory.path().join("archive.sqlite")).unwrap();
        let provider = Arc::new(Capture::new(gated));
        let harness = Arc::new(Harness::new(
            store.clone(),
            provider.clone(),
            ToolRegistry::new(),
            HarnessConfig::default(),
        ));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let input_files = limits.map(|limits| {
            Arc::new(
                InputFiles::new(directory.path(), limits)
                    .unwrap()
                    .with_store(store.clone()),
            )
        });
        let config = WebConfig {
            workspace: directory.path().display().to_string(),
            model: "fixture".into(),
            provider: "demo".into(),
            read_only: true,
        };
        let (shutdown, receive) = oneshot::channel();
        let task = tokio::spawn(serve_with_services(
            listener,
            harness.clone(),
            store,
            config,
            assets,
            WebServices {
                input_files,
                ..Default::default()
            },
            async {
                let _ = receive.await;
            },
        ));
        Self {
            directory,
            base,
            client: Client::new(),
            harness,
            provider,
            shutdown,
            task,
        }
    }
    async fn post(&self, path: &str, value: Value) -> (StatusCode, Value) {
        let response = self
            .client
            .post(format!("{}{path}", self.base))
            .json(&value)
            .send()
            .await
            .unwrap();
        (response.status(), response.json().await.unwrap())
    }
    async fn create(&self) -> String {
        self.post("/api/sessions", json!({})).await.1["id"]
            .as_str()
            .unwrap()
            .into()
    }
    async fn upload(&self, session: &str, name: &str, bytes: Vec<u8>) -> (StatusCode, Value) {
        let response = self
            .client
            .post(format!("{}/api/sessions/{session}/attachments", self.base))
            .query(&[("name", name)])
            .header("content-type", "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .unwrap();
        (response.status(), response.json().await.unwrap())
    }
    async fn stop(self) -> tempfile::TempDir {
        self.shutdown.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        self.directory
    }
}

#[tokio::test]
async fn references_reach_scripted_run_steer_followup_and_survive_archive_reopening() {
    let server = Server::new(Some(InputLimits::default()), true).await;
    let session = server.create().await;
    let small = server
        .upload(&session, "first.txt", b"Small referenced text".to_vec())
        .await;
    assert_eq!(small.0, StatusCode::OK);
    let binary_bytes = b"RAW_BINARY_PAYLOAD_MUST_STAY_OUTSIDE_SQLITE\0\xff".to_vec();
    let binary = server
        .upload(&session, "sample.bin", binary_bytes.clone())
        .await
        .1;
    let large_bytes = b"LARGE_FULL_FILE_PAYLOAD_STAYS_ON_DISK\n".repeat(10_000);
    let large = server
        .upload(&session, "large.txt", large_bytes.clone())
        .await;
    assert_eq!(large.0, StatusCode::OK);
    let second = server
        .upload(&session, "second.txt", b"Steering reference".to_vec())
        .await
        .1;
    let original = "Exact original\r\nprompt 🐾";
    assert_eq!(
        server
            .post(
                &format!("/api/sessions/{session}/run"),
                json!({"text":original,"attachments":[small.1["id"],binary["id"]]})
            )
            .await
            .0,
        StatusCode::OK
    );
    tokio::time::timeout(Duration::from_secs(3), server.provider.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    assert_eq!(
        server
            .post(
                &format!("/api/sessions/{session}/steer"),
                json!({"text":"steering","attachments":[second["id"]]})
            )
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        server
            .post(
                &format!("/api/sessions/{session}/followup"),
                json!({"text":"","attachments":[large.1["id"]]})
            )
            .await
            .0,
        StatusCode::OK
    );
    server.provider.release.add_permits(20);
    tokio::time::timeout(
        Duration::from_secs(5),
        server.harness.wait_for_idle(&session),
    )
    .await
    .unwrap()
    .unwrap();
    let messages = server.harness.store().messages(&session).unwrap();
    let serialized = serde_json::to_string(&messages).unwrap();
    assert!(
        messages
            .iter()
            .any(|message| message.content.starts_with(original))
    );
    assert!(
        serialized.contains("Small referenced text")
            && serialized.contains("Steering reference")
            && serialized.contains("omitted")
    );
    assert!(
        !serialized.contains("LARGE_FULL_FILE_PAYLOAD_STAYS_ON_DISK")
            && !serialized.contains("RAW_BINARY_PAYLOAD_MUST_STAY_OUTSIDE_SQLITE")
    );
    for file in [&small.1, &second, &large.1, &binary] {
        assert!(
            server
                .provider
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request
                    .messages
                    .iter()
                    .any(|message| message.content.contains(file["id"].as_str().unwrap())))
        );
    }
    let response = server
        .client
        .get(format!(
            "{}{}",
            server.base,
            binary["url"].as_str().unwrap()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "application/octet-stream"
    );
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert!(
        response.headers()["content-disposition"]
            .to_str()
            .unwrap()
            .starts_with("attachment;")
    );
    assert_eq!(response.bytes().await.unwrap().as_ref(), binary_bytes);
    let directory = server.stop().await;
    let reopened = Store::open(directory.path().join("archive.sqlite")).unwrap();
    assert_eq!(
        serde_json::to_string(&reopened.messages(&session).unwrap()).unwrap(),
        serialized
    );
    let storage = InputFiles::new(directory.path(), InputLimits::default())
        .unwrap()
        .with_store(reopened.clone());
    assert_eq!(
        storage
            .read(&session, large.1["id"].as_str().unwrap())
            .unwrap()
            .1,
        large_bytes
    );
    assert_eq!(
        storage
            .read(&session, binary["id"].as_str().unwrap())
            .unwrap()
            .1,
        binary_bytes
    );
    let chat = reopened.chat_path(&session).unwrap().unwrap();
    let mut wal = chat.as_os_str().to_os_string();
    wal.push("-wal");
    for path in [chat, std::path::PathBuf::from(wal)] {
        if let Ok(bytes) = std::fs::read(path) {
            assert!(
                !bytes
                    .windows(b"RAW_BINARY_PAYLOAD_MUST_STAY_OUTSIDE_SQLITE".len())
                    .any(|window| window == b"RAW_BINARY_PAYLOAD_MUST_STAY_OUTSIDE_SQLITE")
            );
        }
    }
}

#[tokio::test]
async fn configured_limits_foreign_ids_and_failed_uploads_have_no_admitted_effect() {
    let limits = InputLimits {
        max_file_bytes: 8,
        max_total_bytes: 12,
        max_files: 2,
        prompt_chars: 5,
        ..Default::default()
    };
    let server = Server::new(Some(limits), false).await;
    let session = server.create().await;
    let foreign = server.create().await;
    let config: Value = server
        .client
        .get(format!("{}/api/config", server.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(config["input_limits"]["max_file_bytes"], 8);
    assert_eq!(config["input_limits"]["prompt_chars"], 5);
    let first = server.upload(&session, "one", b"1234567".to_vec()).await.1;
    let second = server.upload(&session, "two", b"abcdefg".to_vec()).await.1;
    let revision = server.harness.store().revision();
    assert_eq!(
        server
            .upload(&session, "oversized", b"123456789".to_vec())
            .await
            .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let stream = futures_util::stream::iter(vec![
        Ok::<Vec<u8>, std::io::Error>(b"1234567".to_vec()),
        Ok(b"890".to_vec()),
    ]);
    let chunked = server
        .client
        .post(format!(
            "{}/api/sessions/{session}/attachments?name=chunked",
            server.base
        ))
        .header("content-type", "application/octet-stream")
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await
        .unwrap();
    assert_eq!(chunked.status(), StatusCode::PAYLOAD_TOO_LARGE);
    for route in ["run", "steer", "followup"] {
        let path = format!("/api/sessions/{session}/{route}");
        for value in [
            json!({"text":"123456"}),
            json!({"text":""}),
            json!({"text":"","attachments":[first["id"],second["id"]]}),
            json!({"text":"","attachments":[first["id"],first["id"]]}),
            json!({"text":"","attachments":["../bad"]}),
        ] {
            assert_eq!(server.post(&path, value).await.0, StatusCode::BAD_REQUEST);
        }
        assert_eq!(
            server
                .post(
                    &format!("/api/sessions/{foreign}/{route}"),
                    json!({"text":"","attachments":[first["id"]]})
                )
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(server.harness.store().revision(), revision);
    assert!(server.provider.requests.lock().unwrap().is_empty());
    let foreign_get = server
        .client
        .get(format!(
            "{}/api/sessions/{foreign}/attachments/{}",
            server.base,
            first["id"].as_str().unwrap()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(foreign_get.status(), StatusCode::NOT_FOUND);
    let wrong_type = server
        .client
        .post(format!(
            "{}/api/sessions/{session}/attachments?name=x",
            server.base
        ))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_type.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let cross_origin = server
        .client
        .post(format!(
            "{}/api/sessions/{session}/attachments?name=x",
            server.base
        ))
        .header("origin", "https://example.com")
        .header("content-type", "application/octet-stream")
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(cross_origin.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        server
            .post(
                &format!("/api/sessions/{session}/run"),
                json!({"text":"🐾".repeat(5)})
            )
            .await
            .0,
        StatusCode::OK
    );
    server.harness.wait_for_idle(&session).await.unwrap();
    server.stop().await;
}

#[tokio::test]
async fn legacy_service_has_default_prompt_limit_and_graceful_unavailable_routes() {
    let server = Server::new(None, false).await;
    let session = server.create().await;
    assert_eq!(
        server.upload(&session, "x", b"x".to_vec()).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        server
            .post(
                &format!("/api/sessions/{session}/run"),
                json!({"text":"text","attachments":[uuid::Uuid::new_v4().to_string()]})
            )
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        server
            .post(
                &format!("/api/sessions/{session}/run"),
                json!({"text":"🐾".repeat(32_001)})
            )
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        server
            .post(
                &format!("/api/sessions/{session}/run"),
                json!({"text":"🐾".repeat(32_000)})
            )
            .await
            .0,
        StatusCode::OK
    );
    server.harness.wait_for_idle(&session).await.unwrap();
    server.stop().await;
}

#[tokio::test]
async fn configurable_prompt_limit_allows_escaped_json_and_default_file_limit_is_enforced() {
    let server = Server::new(
        Some(InputLimits {
            prompt_chars: 50_000,
            ..Default::default()
        }),
        false,
    )
    .await;
    let session = server.create().await;
    // Exercise early Content-Length rejection without concurrently writing a
    // giant request after the server has already returned its error response.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let authority = server.base.strip_prefix("http://").unwrap();
    let mut socket = tokio::net::TcpStream::connect(authority).await.unwrap();
    socket.write_all(format!("POST /api/sessions/{session}/attachments?name=too-large.bin HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/octet-stream\r\nContent-Length: 16777217\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), socket.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(
        String::from_utf8(response)
            .unwrap()
            .starts_with("HTTP/1.1 413")
    );
    let text = "\u{1}".repeat(50_000);
    let value = json!({"text":text});
    assert!(serde_json::to_vec(&value).unwrap().len() > 256 * 1024);
    assert_eq!(
        server
            .post(&format!("/api/sessions/{session}/run"), value)
            .await
            .0,
        StatusCode::OK
    );
    server.harness.wait_for_idle(&session).await.unwrap();
    assert_eq!(
        server.harness.store().messages(&session).unwrap()[0].content,
        text
    );
    server.stop().await;
}
