//! Local transport shape checks and explicitly gated, paid vision acceptance.
//! Live: BP_LIVE_IMAGES=1 OPENROUTER_API_KEY=<environment only>
//! cargo test -p bubblepaw-runner --test provider_images live_ -- --ignored --nocapture
use anyhow::{Result, ensure};
use async_trait::async_trait;
use axum::{Json, Router, extract::State, routing::post};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bubblepaw_core::{
    CancellationToken, Capabilities, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider,
    Role, RunOptions, Store, TextCallback, ToolRegistry,
};
use bubblepaw_runner::{
    artifacts::ArtifactStore,
    codemode::CodeMode,
    images::ImageProvider,
    provider::{ChatCompletionsProvider, MessagesProvider, ResponsesProvider},
    tools::WorkspaceTools,
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

const PNG: &[u8] = include_bytes!("fixtures/image-two-colors.png");
#[derive(Clone, Copy)]
enum Kind {
    Chat,
    Responses,
    Messages,
}

struct Fixture {
    _root: tempfile::TempDir,
    store: Store,
    session: String,
    manager: Arc<CodeMode>,
    harness: Harness,
}
impl Fixture {
    fn new(provider: Arc<dyn Provider>, model: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("diagram.png"), PNG).unwrap();
        let store = Store::in_memory().unwrap();
        let session = store.create_session("image transport").unwrap().id;
        let mut tools = ToolRegistry::new();
        tools.show_always(bubblepaw_runner::tools::ALWAYS_SHOWN_TOOLS.iter().copied());
        WorkspaceTools::new(root.path())
            .unwrap()
            .register(&mut tools, true)
            .unwrap();
        let artifacts =
            Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap());
        let manager = Arc::new(CodeMode::new(root.path().to_owned(), artifacts).unwrap());
        manager.register(&mut tools).unwrap();
        let harness = Harness::new(
            store.clone(),
            Arc::new(ImageProvider::new(provider, store.clone())),
            tools,
            HarnessConfig {
                execution_mode: bubblepaw_core::ExecutionMode::Js,
                model: model.into(),
                max_rounds: Some(4),
                ..Default::default()
            },
        );
        Self {
            _root: root,
            store,
            session,
            manager,
            harness,
        }
    }
}

fn provider(kind: Kind, key: String, base: &str) -> Arc<dyn Provider> {
    match kind {
        Kind::Chat => Arc::new(ChatCompletionsProvider::api_key(key, base).unwrap()),
        Kind::Responses => Arc::new(ResponsesProvider::api_key(key, base).unwrap()),
        Kind::Messages => Arc::new(MessagesProvider::api_key(key, base).unwrap()),
    }
}
fn reply(kind: Kind, first: bool) -> (&'static str, String) {
    let arguments = json!({"code":"await tools.read({path:'diagram.png'}); await tools.read({path:'diagram.png'}); return 'image read';"});
    let second = json!({"code":"return 'ordinary text result';"});
    match kind {
        Kind::Chat => ("application/json", json!({"choices":[{"message":if first {
            json!({"role":"assistant","content":null,"tool_calls":[{"id":"image","type":"function","function":{"name":"js","arguments":arguments.to_string()}},{"id":"text","type":"function","function":{"name":"js","arguments":second.to_string()}}]})
        } else { json!({"role":"assistant","content":"done"}) },"finish_reason":if first {"tool_calls"} else {"stop"}}],"usage":{"prompt_tokens":2,"completion_tokens":2}}).to_string()),
        Kind::Messages => ("application/json", json!({"type":"message","role":"assistant","content":if first {
            json!([{"type":"tool_use","id":"image","name":"js","input":arguments},{"type":"tool_use","id":"text","name":"js","input":second}])
        } else { json!([{"type":"text","text":"done"}]) },"stop_reason":if first {"tool_use"} else {"end_turn"},"usage":{"input_tokens":2,"output_tokens":2}}).to_string()),
        Kind::Responses => {
            let output = if first { vec![json!({"type":"function_call","call_id":"image","name":"js","arguments":arguments.to_string()}),json!({"type":"function_call","call_id":"text","name":"js","arguments":second.to_string()})] } else { vec![json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]})] };
            let mut events: Vec<Value> = output.iter().enumerate().map(|(index,item)| json!({"type":"response.output_item.done","output_index":index,"item":item})).collect();
            if !first { events.insert(0,json!({"type":"response.output_text.delta","delta":"done"})); }
            events.push(json!({"type":"response.completed","response":{"status":"completed","output":output,"usage":{"input_tokens":2,"output_tokens":2}}}));
            ("text/event-stream", events.iter().map(|event| format!("data: {event}\n\n")).collect())
        }
    }
}
#[derive(Clone)]
struct Server {
    kind: Kind,
    requests: Arc<Mutex<Vec<Value>>>,
}
async fn shapes(kind: Kind) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let state = Server {
        kind,
        requests: requests.clone(),
    };
    let route = match kind {
        Kind::Chat => "/chat/completions",
        Kind::Responses => "/responses",
        Kind::Messages => "/messages",
    };
    let app = Router::new()
        .route(
            route,
            post(
                |State(state): State<Server>, Json(body): Json<Value>| async move {
                    let first = {
                        let mut requests = state.requests.lock().unwrap();
                        requests.push(body);
                        requests.len() == 1
                    };
                    let (mime, body) = reply(state.kind, first);
                    ([("content-type", mime)], body)
                },
            ),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let fixture = Fixture::new(provider(kind, "synthetic-key".into(), &base), "fixture");
    let run = fixture
        .harness
        .prompt(
            &fixture.session,
            "Inspect the diagram.",
            RunOptions::default(),
        )
        .await
        .unwrap();
    assert!(
        run.error.is_none(),
        "local image run failed: {:?}",
        run.error
    );
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let body = &requests[1];
        let encoded = STANDARD.encode(PNG);
        assert_eq!(body.to_string().matches(&encoded).count(), 1);
        assert!(!body.to_string().contains("BubblePawImageWire"));
        match kind {
            Kind::Chat => {
                let messages = body["messages"].as_array().unwrap();
                let tools: Vec<_> = messages
                    .iter()
                    .enumerate()
                    .filter(|(_, message)| message["role"] == "tool")
                    .collect();
                assert_eq!(tools.len(), 2);
                assert_eq!(tools[0].1["tool_call_id"], "image");
                assert!(tools[0].1["content"].is_string());
                assert_eq!(tools[1].1["tool_call_id"], "text");
                assert_eq!(tools[1].0, tools[0].0 + 1);
                let image = &messages[tools[1].0 + 1];
                assert_eq!(image["role"], "user");
                assert_eq!(image["content"][1]["type"], "image_url");
                assert_eq!(
                    image["content"][1]["image_url"]["url"],
                    format!("data:image/png;base64,{encoded}")
                );
            }
            Kind::Responses => {
                let results: Vec<_> = body["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|item| item["type"] == "function_call_output")
                    .collect();
                assert_eq!(results.len(), 2);
                assert_eq!(results[0]["call_id"], "image");
                assert_eq!(results[0]["output"][0]["type"], "input_text");
                assert_eq!(results[0]["output"][1]["type"], "input_image");
                assert_eq!(
                    results[0]["output"][1]["image_url"],
                    format!("data:image/png;base64,{encoded}")
                );
                assert!(results[1]["output"].is_string());
            }
            Kind::Messages => {
                let results: Vec<_> = body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .flat_map(|message| message["content"].as_array().unwrap())
                    .filter(|block| block["type"] == "tool_result")
                    .collect();
                assert_eq!(results.len(), 2);
                assert_eq!(results[0]["tool_use_id"], "image");
                assert_eq!(results[0]["content"][0]["type"], "text");
                assert_eq!(results[0]["content"][1]["type"], "image");
                assert_eq!(
                    results[0]["content"][1]["source"],
                    json!({"type":"base64","media_type":"image/png","data":encoded})
                );
                assert!(results[1]["content"].is_string());
            }
        }
        for message in fixture.store.messages(&fixture.session).unwrap() {
            assert!(!message.content.contains(&STANDARD.encode(PNG)));
        }
    }
    fixture.manager.shutdown().await;
    server.abort();
}
#[tokio::test]
async fn chat_image_follows_complete_parallel_tool_group() {
    shapes(Kind::Chat).await;
}
#[tokio::test]
async fn responses_tool_output_contains_native_image_parts() {
    shapes(Kind::Responses).await;
}
#[tokio::test]
async fn messages_tool_result_contains_native_image_blocks() {
    shapes(Kind::Messages).await;
}

#[test]
fn configured_image_capability_is_pinned_to_the_live_provider_generation() {
    use bubblepaw_runner::{
        config::{EffectiveConfig, LoadOptions},
        provider::LiveProviderConfig,
    };
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.toml");
    let config = |enabled: bool| {
        EffectiveConfig::load_with_file(LoadOptions { workspace:root.path().to_owned(),home:None,custom_file:Some(path.clone()),env:Vec::new(),cli:Vec::new() },&path,&format!("[model]\nprovider='demo'\nmain='demo/fixture'\n[providers.demo]\nkind='scripted'\nmodels=[{{id='fixture',image_input={enabled}}}]\n")).unwrap()
    };
    let live = Arc::new(
        LiveProviderConfig::new(config(false))
            .unwrap()
            .with_image_store(Store::in_memory().unwrap()),
    );
    let (model, old) = live.request_binding();
    assert!(!old.capabilities(&model).image_input);
    let prepared = live.prepare(config(true)).unwrap();
    live.apply_prepared(prepared);
    let (_, new) = live.request_binding();
    assert!(new.capabilities(&model).image_input);
    assert!(!old.capabilities(&model).image_input);
}

struct Limited {
    inner: Arc<dyn Provider>,
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl Provider for Limited {
    fn capabilities(&self, model: &str) -> Capabilities {
        self.inner.capabilities(model)
    }
    async fn generate(
        &self,
        mut request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let step = self.calls.fetch_add(1, Ordering::SeqCst);
        ensure!(step < 4, "live vision request limit exceeded");
        let read_signature = request
            .tools
            .iter()
            .any(|tool| tool.description.contains("tools.read({path"));
        let encoded = STANDARD.encode(PNG);
        let image_parts: usize = request
            .messages
            .iter()
            .map(|message| message.content.matches(&encoded).count())
            .sum();
        request.max_output_tokens = Some(512);
        let response = self.inner.generate(request, on_text, cancellation).await?;
        eprintln!(
            "live image step: step={} read_signature={read_signature} image_parts={image_parts} tools={:?} text={}",
            step + 1,
            response
                .tool_calls
                .iter()
                .map(|call| call.name.as_str())
                .collect::<Vec<_>>(),
            response.text.chars().take(512).collect::<String>()
        );
        Ok(response)
    }
}
async fn live(kind: Kind) {
    if std::env::var("BP_LIVE_IMAGES").as_deref() != Ok("1") {
        eprintln!("SKIP: BP_LIVE_IMAGES=1 required");
        return;
    }
    let Ok(key) = std::env::var("OPENROUTER_API_KEY") else {
        eprintln!("SKIP: OPENROUTER_API_KEY required");
        return;
    };
    let model = std::env::var("BP_LIVE_IMAGES_MODEL").unwrap_or_else(|_| {
        match kind {
            Kind::Responses => "openai/gpt-4.1-nano",
            Kind::Chat | Kind::Messages => "google/gemini-2.5-flash-lite",
        }
        .into()
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let inner = Arc::new(Limited {
        inner: provider(kind, key, "https://openrouter.ai/api/v1"),
        calls: calls.clone(),
    });
    let fixture = Fixture::new(inner, &model);
    let result=fixture.harness.prompt(&fixture.session,"Use js and tools.read({path:'diagram.png'}) to inspect diagram.png. The read tool automatically attaches the image to your js tool result; you can then see its content. State the color on its left half and the color on its right half. Do not infer colors from the filename. Keep the final answer to one short sentence.",RunOptions { max_tool_calls:Some(8),..Default::default() }).await.unwrap();
    assert!(
        result.error.is_none(),
        "live image run failed: {:?}",
        result.error
    );
    let text = result.text.unwrap_or_default().to_lowercase();
    assert!(
        fixture
            .store
            .messages(&fixture.session)
            .unwrap()
            .iter()
            .any(|message| message.role == Role::Tool
                && !bubblepaw_runner::images::split_refs(&message.content)
                    .1
                    .is_empty()),
        "live model did not read the image through js; answer: {text}"
    );
    let orientation = text
        .find("left")
        .zip(text.find("right"))
        .zip(text.find("red").zip(text.find("blue")))
        .is_some_and(|((left, right), (red, blue))| (left < right) == (red < blue));
    assert!(orientation, "unexpected image orientation answer: {text}");
    eprintln!(
        "live image acceptance: model={model} provider_steps={} answer={text}",
        calls.load(Ordering::SeqCst)
    );
    fixture.manager.shutdown().await;
}
#[tokio::test]
#[ignore = "paid OpenRouter image acceptance; requires explicit environment gate"]
async fn live_openrouter_chat_read_js_image() {
    live(Kind::Chat).await;
}
#[tokio::test]
#[ignore = "paid OpenRouter image acceptance; requires explicit environment gate"]
async fn live_openrouter_responses_read_js_image() {
    live(Kind::Responses).await;
}
