use anyhow::Result;
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bubblepaw_core::{
    CancellationToken, Capabilities, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider,
    Role, RunOptions, Store, TextCallback, ToolCall, ToolRegistry,
};
use bubblepaw_runner::{
    artifacts::ArtifactStore,
    codemode::CodeMode,
    images::{ImageProvider, MAX_IMAGE_BYTES, split_refs, store_image},
    tools::WorkspaceTools,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const PNG: &[u8] = include_bytes!("fixtures/image-two-colors.png");

struct Cells {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    vision: bool,
}
#[async_trait]
impl Provider for Cells {
    fn capabilities(&self, _: &str) -> Capabilities {
        Capabilities {
            image_input: self.vision,
            ..Default::default()
        }
    }
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request.clone());
        if let Some(arguments) = request
            .messages
            .iter()
            .rev()
            .take_while(|message| message.role == Role::User)
            .find_map(|message| serde_json::from_str::<Value>(&message.content).ok())
        {
            return Ok(ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "js".into(),
                    arguments,
                }],
                ..Default::default()
            });
        }
        Ok(ModelResponse {
            text: "done".into(),
            ..Default::default()
        })
    }
}

struct Fixture {
    root: tempfile::TempDir,
    store: Store,
    harness: Harness,
    manager: Arc<CodeMode>,
    session: String,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}
impl Fixture {
    fn new(vision: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path().join("record.db")).unwrap();
        let session = store.create_session("images").unwrap().id;
        let mut tools = ToolRegistry::new();
        let workspace = WorkspaceTools::new(root.path()).unwrap();
        workspace.register(&mut tools, true).unwrap();
        Arc::new(bubblepaw_runner::tools::ProcessTools::new(workspace))
            .register_wait(&mut tools)
            .unwrap();
        let artifacts =
            Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap());
        let manager = Arc::new(CodeMode::new(root.path().to_owned(), artifacts).unwrap());
        manager.register(&mut tools).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let inner = Arc::new(Cells {
            requests: requests.clone(),
            vision,
        });
        let harness = Harness::new(
            store.clone(),
            Arc::new(ImageProvider::new(inner, store.clone())),
            tools,
            HarnessConfig {
                execution_mode: bubblepaw_core::ExecutionMode::Js,
                ..Default::default()
            },
        );
        Self {
            root,
            store,
            harness,
            manager,
            session,
            requests,
        }
    }
    async fn cell(&self, code: &str) -> String {
        let run = self
            .harness
            .prompt(
                &self.session,
                json!({"code":code}).to_string(),
                RunOptions::default(),
            )
            .await
            .unwrap();
        assert!(
            run.error.is_none(),
            "image fixture run failed: {:?}",
            run.error
        );
        self.store
            .messages(&self.session)
            .unwrap()
            .into_iter()
            .rev()
            .find(|message| message.role == Role::Tool)
            .unwrap()
            .content
    }
}

#[tokio::test]
async fn image_reads_attach_without_return_and_deduplicate_across_paths() {
    let fixture = Fixture::new(true);
    std::fs::write(fixture.root.path().join("image.png"), PNG).unwrap();
    std::fs::write(fixture.root.path().join("copy.png"), PNG).unwrap();
    let result = fixture.cell("await Promise.all([tools.read({path:'image.png'}),tools.read({path:'copy.png'}),tools.read({path:'image.png'})]); return 'look at the image';").await;
    let (text, images) = split_refs(&result);
    assert!(text.contains("look at the image"));
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].mime_type, "image/png");
    assert_eq!(
        fixture
            .store
            .attachment(&fixture.session, &images[0].id)
            .unwrap()
            .unwrap()
            .1,
        PNG
    );
    let encoded = STANDARD.encode(PNG);
    assert!(!result.contains(&encoded));
    {
        let requests = fixture.requests.lock().unwrap();
        let sent = requests
            .last()
            .unwrap()
            .messages
            .iter()
            .find(|message| message.role == Role::Tool)
            .unwrap();
        assert_eq!(sent.content.matches(&encoded).count(), 1);
    }
    fixture.manager.shutdown().await;
}

#[tokio::test]
async fn supported_formats_and_four_image_cap_are_explicit() {
    let fixture = Fixture::new(true);
    let files: [(&str, &[u8]); 5] = [
        ("a.png", PNG),
        ("b.jpg", b"\xff\xd8\xffsynthetic-jpeg"),
        ("c.gif", b"GIF89asynthetic-gif"),
        ("d.webp", b"RIFF\x08\0\0\0WEBPsynthetic-webp"),
        ("e.png", b"\x89PNG\r\n\x1a\nother-png"),
    ];
    for (name, bytes) in files {
        std::fs::write(fixture.root.path().join(name), bytes).unwrap();
    }
    let result = fixture.cell("for (const path of ['a.png','b.jpg','c.gif','d.webp','e.png']) await tools.read({path}); return 'checked';").await;
    let (text, images) = split_refs(&result);
    assert_eq!(
        images
            .iter()
            .map(|image| image.mime_type.as_str())
            .collect::<Vec<_>>(),
        ["image/png", "image/jpeg", "image/gif", "image/webp"]
    );
    assert!(text.contains("at most 4"));
    assert!(text.contains("omitted"));
    fixture.manager.shutdown().await;
}

#[tokio::test]
async fn image_limit_is_exactly_five_million_bytes_and_does_not_resize() {
    let fixture = Fixture::new(false);
    let mut bytes = PNG.to_vec();
    bytes.resize(MAX_IMAGE_BYTES, 0);
    std::fs::write(fixture.root.path().join("limit.png"), &bytes).unwrap();
    bytes.push(0);
    std::fs::write(fixture.root.path().join("over.png"), &bytes).unwrap();
    let result = fixture
        .cell("return await tools.read({path:'limit.png'});")
        .await;
    assert_eq!(split_refs(&result).1[0].bytes, 5_000_000);
    let result = fixture
        .cell("return await tools.read({path:'over.png'});")
        .await;
    assert!(split_refs(&result).1.is_empty());
    assert!(result.contains("5 MB"));
    assert!(result.contains("no resize"));
    fixture.manager.shutdown().await;
}

#[tokio::test]
async fn nonvision_models_receive_a_short_note_without_bytes() {
    let fixture = Fixture::new(false);
    std::fs::write(fixture.root.path().join("image.png"), PNG).unwrap();
    let canonical = fixture
        .cell("await tools.read({path:'image.png'}); return 'done';")
        .await;
    assert_eq!(split_refs(&canonical).1.len(), 1);
    {
        let requests = fixture.requests.lock().unwrap();
        let sent = requests
            .last()
            .unwrap()
            .messages
            .iter()
            .find(|message| message.role == Role::Tool)
            .unwrap();
        assert!(sent.content.contains("Image input is unavailable"));
        assert!(!sent.content.contains(&STANDARD.encode(PNG)));
        assert!(!sent.content.contains("BubblePawImages"));
    }
    fixture.manager.shutdown().await;
}

#[tokio::test]
async fn truncated_js_text_and_budget_footer_preserve_image_refs() {
    let fixture = Fixture::new(true);
    std::fs::write(fixture.root.path().join("image.png"), PNG).unwrap();
    let run = fixture.harness.prompt(&fixture.session, json!({"code":"await tools.read({path:'image.png'}); return 'x'.repeat(200000);", "maxOutputTokens":256}).to_string(), RunOptions { max_tool_calls: Some(3), ..Default::default() }).await.unwrap();
    assert!(
        run.error.is_none(),
        "image fixture run failed: {:?}",
        run.error
    );
    let messages = fixture.store.messages(&fixture.session).unwrap();
    let canonical = messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .unwrap();
    let (text, images) = split_refs(&canonical.content);
    assert_eq!(images.len(), 1);
    assert!(text.contains("truncated"));
    assert!(text.ends_with("1 tool calls left"));
    assert!(canonical.content.len() < 4096);
    {
        let requests = fixture.requests.lock().unwrap();
        assert!(
            requests
                .last()
                .unwrap()
                .messages
                .iter()
                .any(|message| message.role == Role::Tool
                    && message.content.contains(&STANDARD.encode(PNG)))
        );
    }
    fixture.manager.shutdown().await;
}

#[tokio::test]
async fn repeated_reads_across_cells_send_one_image_per_prepared_request() {
    let fixture = Fixture::new(true);
    std::fs::write(fixture.root.path().join("image.png"), PNG).unwrap();
    fixture
        .cell("await tools.read({path:'image.png'}); return 'first';")
        .await;
    fixture
        .cell("await tools.read({path:'image.png'}); return 'again';")
        .await;
    {
        let requests = fixture.requests.lock().unwrap();
        let last = requests.last().unwrap();
        let history = last
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<String>();
        assert_eq!(history.matches(&STANDARD.encode(PNG)).count(), 1);
        assert!(history.contains("Image already provided earlier"));
    }
    fixture.manager.shutdown().await;
}

#[tokio::test]
async fn fork_refs_retain_the_original_attachment_session() {
    let fixture = Fixture::new(true);
    std::fs::write(fixture.root.path().join("image.png"), PNG).unwrap();
    fixture
        .cell("await tools.read({path:'image.png'}); return 'first';")
        .await;
    let fork = fixture
        .store
        .fork(&fixture.session, "fork with image")
        .unwrap();
    let messages = fixture.store.messages(&fork.id).unwrap();
    let reference = messages
        .iter()
        .find_map(|message| split_refs(&message.content).1.into_iter().next())
        .unwrap();
    assert_eq!(reference.session_id, fixture.session);
    assert!(
        fixture
            .store
            .attachment(&fork.id, &reference.id)
            .unwrap()
            .is_none()
    );
    fixture
        .harness
        .prompt(
            &fork.id,
            "Describe the earlier image again.",
            RunOptions::default(),
        )
        .await
        .unwrap();
    {
        let requests = fixture.requests.lock().unwrap();
        let latest = requests.last().unwrap();
        assert!(
            latest
                .messages
                .iter()
                .any(|message| message.content.contains(&STANDARD.encode(PNG)))
        );
        assert!(latest.messages.iter().all(|message| {
            !message
                .content
                .contains("canonical attachment was not found")
        }));
    }
    fixture.manager.shutdown().await;
}

#[tokio::test]
async fn background_image_read_is_attached_when_its_environment_is_inspected() {
    use std::time::Duration;
    let fixture = Fixture::new(true);
    std::fs::write(fixture.root.path().join("image.png"), PNG).unwrap();
    fixture.harness.start(&fixture.session,json!({"code":"await wait({ms:60000}); await tools.read({path:'image.png'}); return 'late image';"}).to_string(),RunOptions::default()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if fixture.manager.status(&fixture.session)["environments"]
                .as_array()
                .is_some_and(|envs| {
                    envs.iter()
                        .any(|env| env["name"] == "main" && env["time_waiting"] == 1)
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("background cell did not enter time wait");
    assert_eq!(fixture.manager.background_waits(&fixture.session), 1);
    let run = fixture
        .harness
        .wait_for_idle(&fixture.session)
        .await
        .unwrap();
    assert!(
        run.error.is_none(),
        "background fixture run failed: {:?}",
        run.error
    );
    fixture
        .manager
        .control(
            &fixture.session,
            json!({"action":"wake","environment":"main"}),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if fixture.manager.status(&fixture.session)["environments"]
                .as_array()
                .is_some_and(|envs| {
                    envs.iter()
                        .any(|env| env["name"] == "main" && env["status"] == "idle")
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("background image read did not finish");
    fixture
        .manager
        .control(
            &fixture.session,
            json!({"action":"create","environment":"inspector"}),
        )
        .unwrap();
    let run=fixture.harness.prompt(&fixture.session,json!({"environment":"inspector","code":"return await tools.environments({action:'inspect',environment:'main'});"}).to_string(),RunOptions::default()).await.unwrap();
    assert!(
        run.error.is_none(),
        "inspection fixture run failed: {:?}",
        run.error
    );
    let messages = fixture.store.messages(&fixture.session).unwrap();
    let result = messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Tool)
        .unwrap();
    assert_eq!(split_refs(&result.content).1.len(), 1);
    assert!(
        fixture
            .requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .messages
            .iter()
            .any(|message| message.content.contains(&STANDARD.encode(PNG)))
    );
    fixture.manager.shutdown().await;
}

#[path = "images/blob_replay.rs"]
mod blob_replay;
