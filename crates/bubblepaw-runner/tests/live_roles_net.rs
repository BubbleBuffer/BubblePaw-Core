use axum::{
    Json, Router,
    extract::State,
    http::HeaderMap,
    routing::{get, post},
};
use bubblepaw_core::{
    CancellationToken, HarnessConfig, RunOptions, Store, ToolContext, ToolRegistry,
};
use bubblepaw_runner::{
    artifacts::ArtifactStore,
    codemode::CodeMode,
    config::{EffectiveConfig, LoadOptions},
    episode_models::{
        ChapterInput, ClassicalInput, EpisodeModelConfig, EpisodeModels, HttpEpisodeModels,
        SplitInput, SplitOption, SummaryInput,
    },
    net::{NetClient, WebConfig, register_fetch_search_tools_with_store_live},
    provider::{LiveProviderConfig, ScriptedProvider},
    research::research_profile_with_tools_live,
};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Notify;

#[derive(Default)]
struct Fixture {
    label: String,
    requests: Mutex<Vec<Value>>,
    entered: Notify,
    release: Notify,
    hold_model: bool,
    hold_search: bool,
    crawl_authenticated: Mutex<Vec<bool>>,
}
async fn serve(
    label: &str,
    hold_model: bool,
    hold_search: bool,
) -> (String, Arc<Fixture>, tokio::task::JoinHandle<()>) {
    let fixture = Arc::new(Fixture {
        label: label.into(),
        hold_model,
        hold_search,
        ..Default::default()
    });
    let router = Router::new()
        .route("/chat/completions", post(|State(fixture): State<Arc<Fixture>>, Json(body): Json<Value>| async move {
            fixture.requests.lock().unwrap().push(body.clone());
            let model = body["model"].as_str().unwrap();
            if fixture.hold_model && model == "summary-old" {
                fixture.entered.notify_one();
                fixture.release.notified().await;
            }
            let content = if model.starts_with("decision") { "{\"choice\":\"1\"}".into() }
                else { format!("# {}\nFixture summary.\nRelated: none", fixture.label) };
            Json(json!({"choices":[{"message":{"content":content},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}))
        }))
        .route("/search", get(|State(fixture): State<Arc<Fixture>>| async move {
            fixture.requests.lock().unwrap().push(json!({"type":"search"}));
            if fixture.hold_search && fixture.requests.lock().unwrap().len() == 1 {
                fixture.entered.notify_one();
                fixture.release.notified().await;
            }
            Json(json!({"results":[{"title":fixture.label,"url":"https://example.com/source","content":"fixture evidence"}]}))
        }))
        .route("/crawl", post(|State(fixture): State<Arc<Fixture>>, headers: HeaderMap, Json(body): Json<Value>| async move {
            fixture.requests.lock().unwrap().push(body.clone());
            fixture.crawl_authenticated.lock().unwrap().push(
                headers.get("authorization").and_then(|value| value.to_str().ok())
                    == Some("Bearer invocation-fixture-token")
            );
            Json(json!({"success":true,"results":[{"success":true,"redirected_url":body["urls"][0],"markdown":{"fit_markdown":format!("# {}\nFixture page.", fixture.label)}}]}))
        }))
        .with_state(fixture.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (base, fixture, server)
}

fn configuration(root: &Path, base: &str, suffix: &str, extra: &str) -> EffectiveConfig {
    configuration_with_token(root, base, suffix, extra, "invocation-fixture-token")
}
fn configuration_with_token(
    root: &Path,
    base: &str,
    suffix: &str,
    extra: &str,
    crawl_token: &str,
) -> EffectiveConfig {
    let path = root.join("config.toml");
    let (effort, max_tokens, repeats) = if suffix == "old" {
        ("low", 111, 1)
    } else {
        ("high", 123, 2)
    };
    // Loopback credentials belong to this invocation snapshot. Tests never
    // depend on credentials or PATH inherited from the test runner process.
    let text = format!(
        r#"
[model]
main = "fixture/main"
[providers.fixture]
kind = "chat-completions"
base_url = "{base}"
key_env = "BP_LIVE_ROLES_API_KEY"
[roles.summary]
models = ["fixture/summary-{suffix}"]
reasoning_effort = "{effort}"
max_output_tokens = {max_tokens}
[roles.chapter]
models = ["fixture/chapter-{suffix}"]
reasoning_effort = "medium"
[roles.decider]
models = ["llm/fixture/decision-{suffix}"]
repeats = {repeats}
threshold = 1.0
[roles.approver]
models = ["llm/fixture/approval-{suffix}"]
[web]
enabled = true
allow_private = true
search = ["searxng:{base}"]
fetch_backend = ["crawl4ai:{base}"]
cache_ttl_secs = 86400
max_searches = 3
max_fetches = 2
{extra}
"#
    );
    EffectiveConfig::load_with_file(
        LoadOptions {
            workspace: root.into(),
            home: None,
            custom_file: Some(path.clone()),
            env: vec![
                ("BP_LIVE_ROLES_API_KEY".into(), "fixture-api-key".into()),
                ("CRAWL4AI_API_TOKEN".into(), crawl_token.into()),
            ],
            cli: Vec::new(),
        },
        &path,
        &text,
    )
    .unwrap()
}
fn web_configuration(live: &LiveProviderConfig) -> WebConfig {
    let generation = live.snapshot();
    generation
        .settings
        .web
        .net_config_with_env(&|name| generation.effective.environment(name))
        .unwrap()
        .unwrap()
}
fn apply(live: &LiveProviderConfig, effective: EffectiveConfig) {
    let _update = live.lock_update().unwrap();
    let prepared = live.prepare(effective).unwrap();
    live.apply_prepared(prepared);
}
fn classical() -> ClassicalInput {
    ClassicalInput {
        session_id: "fixture".into(),
        previous_summary: None,
        latest_user_request: None,
        span_text: "Fixture history".into(),
    }
}

#[tokio::test]
async fn episode_roles_and_registry_switch_together_while_inflight_request_keeps_old_generation() {
    let root = tempfile::tempdir().unwrap();
    let (old_base, old_fixture, old_server) = serve("old", true, false).await;
    let (new_base, new_fixture, new_server) = serve("new", false, false).await;
    let live = Arc::new(
        LiveProviderConfig::new(configuration(root.path(), &old_base, "old", "")).unwrap(),
    );
    let roles = live.snapshot().settings.roles.clone();
    let models = Arc::new(
        HttpEpisodeModels::from_live(
            EpisodeModelConfig {
                roles,
                retry_base_delay: Duration::ZERO,
            },
            live.clone(),
            None,
        )
        .unwrap(),
    );
    let inflight_models = models.clone();
    let inflight = tokio::spawn(async move {
        inflight_models
            .summarize_classical(classical())
            .await
            .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(5), old_fixture.entered.notified())
        .await
        .unwrap();
    apply(&live, configuration(root.path(), &new_base, "new", ""));
    let fresh = models.summarize_classical(classical()).await.unwrap();
    assert_eq!(fresh.model, "summary-new");
    assert!(fresh.summary.contains("new"));
    old_fixture.release.notify_one();
    let old = inflight.await.unwrap();
    assert_eq!(old.model, "summary-old");
    assert!(old.summary.contains("old"));
    let summary = models
        .summarize(SummaryInput {
            focus: None,
            session_id: "fixture".into(),
            previous_summary: None,
            turn_request: None,
            episode_text: "Fixture episode".into(),
            cap: 1000,
        })
        .await
        .unwrap();
    assert_eq!(summary.model, "summary-new");
    let chapter = models
        .close_chapter(ChapterInput {
            session_id: "fixture".into(),
            episodes: vec![summary.summary],
            previous_chapters: Vec::new(),
            typed_user_texts: Vec::new(),
            task_instructions: Vec::new(),
        })
        .await
        .unwrap();
    assert_eq!(chapter.model, "chapter-new");
    let decision = models
        .decide(SplitInput {
            session_id: "fixture".into(),
            open_chapter: "Old task".into(),
            options: vec![SplitOption {
                message_id: "m1".into(),
                user: "New task".into(),
                agent_before: String::new(),
            }],
        })
        .await
        .unwrap();
    assert_eq!(decision.choice.as_deref(), Some("m1"));
    let requests = new_fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[0]["reasoning_effort"], "high");
    assert_eq!(requests[0]["max_tokens"], 123);
    assert_eq!(requests[2]["reasoning_effort"], "medium");
    assert_eq!(requests[3]["model"], "decision-new");
    assert_eq!(requests[4]["model"], "decision-new");
    assert_eq!(old_fixture.requests.lock().unwrap().len(), 1);
    assert_eq!(
        old_fixture.requests.lock().unwrap()[0]["reasoning_effort"],
        "low"
    );
    assert_eq!(old_fixture.requests.lock().unwrap()[0]["max_tokens"], 111);
    old_server.abort();
    new_server.abort();
}

#[tokio::test]
async fn live_search_changes_do_not_reuse_removed_backend_cache_or_reset_budgets() {
    let root = tempfile::tempdir().unwrap();
    let (old_base, old_fixture, old_server) = serve("old", false, true).await;
    let (new_base, new_fixture, new_server) = serve("new", false, false).await;
    let live = Arc::new(
        LiveProviderConfig::new(configuration(root.path(), &old_base, "old", "")).unwrap(),
    );
    let web = web_configuration(&live);
    let store = Store::in_memory().unwrap();
    let client = NetClient::with_store_live(&web, &store, live.clone()).unwrap();
    let inflight_client = client.clone();
    let inflight = tokio::spawn(async move {
        inflight_client
            .search_served("same query", 8)
            .await
            .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(5), old_fixture.entered.notified())
        .await
        .unwrap();
    apply(&live, configuration(root.path(), &new_base, "new", ""));
    assert_eq!(
        client.search_served("same query", 8).await.unwrap().results[0].title,
        "new"
    );
    old_fixture.release.notify_one();
    assert_eq!(inflight.await.unwrap().results[0].title, "old");
    assert_eq!(
        client.search_served("same query", 8).await.unwrap().results[0].title,
        "new"
    );
    client.search_served("third miss", 8).await.unwrap();
    apply(&live, configuration(root.path(), &new_base, "newer", ""));
    let error = client.search_served("fourth miss", 8).await.unwrap_err();
    assert!(error.to_string().contains("search budget exhausted"));
    assert_eq!(new_fixture.requests.lock().unwrap().len(), 2);
    old_server.abort();
    new_server.abort();
}

#[tokio::test]
async fn live_fetch_changes_preserve_source_ids_and_shared_miss_budget() {
    let root = tempfile::tempdir().unwrap();
    let (old_base, old_fixture, old_server) = serve("old", false, false).await;
    let (new_base, new_fixture, new_server) = serve("new", false, false).await;
    let live = Arc::new(
        LiveProviderConfig::new(configuration(root.path(), &old_base, "old", "")).unwrap(),
    );
    let web = web_configuration(&live);
    let store = Store::in_memory().unwrap();
    let session = store.create_session("fixture").unwrap().id;
    let mut tools = ToolRegistry::new();
    register_fetch_search_tools_with_store_live(&mut tools, &web, &store, live.clone()).unwrap();
    let tool = tools.get_host("web_fetch").unwrap();
    let context = || ToolContext {
        session_id: session.clone(),
        run_id: "fixture".into(),
        invocation_id: uuid::Uuid::new_v4().to_string(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    };
    let first = tool
        .invoke(context(), json!({"url":"https://example.com/page"}))
        .await
        .unwrap();
    let first: Value = serde_json::from_str(&first.content).unwrap();
    assert!(first["content"].as_str().unwrap().contains("old"));
    apply(&live, configuration(root.path(), &new_base, "new", ""));
    let second = tool
        .invoke(context(), json!({"id":first["id"]}))
        .await
        .unwrap();
    let second: Value = serde_json::from_str(&second.content).unwrap();
    assert!(second["content"].as_str().unwrap().contains("new"));
    assert_eq!(first["id"], second["id"]);
    assert_eq!(first["source"], second["source"]);
    tool.invoke(context(), json!({"id":first["id"]}))
        .await
        .unwrap();
    let error = tool
        .invoke(context(), json!({"url":"https://example.com/uncached"}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("fetch budget exhausted"));
    assert_eq!(old_fixture.requests.lock().unwrap().len(), 1);
    assert_eq!(new_fixture.requests.lock().unwrap().len(), 1);
    old_server.abort();
    new_server.abort();
}

#[tokio::test]
async fn existing_research_profile_uses_live_backends_even_when_parent_tools_are_hidden() {
    let root = tempfile::tempdir().unwrap();
    let (old_base, _, old_server) = serve("old", false, false).await;
    let (new_base, new_fixture, new_server) = serve("new", false, false).await;
    let hidden = "[web.expose]\nfetch = false\nsearch = false";
    let live = Arc::new(
        LiveProviderConfig::new(configuration(root.path(), &old_base, "old", hidden)).unwrap(),
    );
    let web = web_configuration(&live);
    let store = Store::in_memory().unwrap();
    let repl = Arc::new(
        CodeMode::new(
            root.path().into(),
            Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap()),
        )
        .unwrap(),
    );
    let mut tools = ToolRegistry::new();
    register_fetch_search_tools_with_store_live(&mut tools, &web, &store, live.clone()).unwrap();
    assert!(tools.get_host("web_search").is_none());
    let profile = research_profile_with_tools_live(
        store.clone(),
        Arc::new(ScriptedProvider::new("unused")),
        &tools,
        &repl,
        &HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            ..Default::default()
        },
        &web,
        live.clone(),
    )
    .unwrap();
    apply(&live, configuration(root.path(), &new_base, "new", hidden));
    let session = store.create_session("research").unwrap().id;
    let result = profile
        .harness
        .prompt(
            &session,
            "fixture research",
            RunOptions {
                script: Some(
                    "return await tools.web_search({query:'hidden tool fixture'});".into(),
                ),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result.status, "completed", "{:?}", result.error);
    let result: Value = serde_json::from_str(result.text.as_deref().unwrap()).unwrap();
    assert_eq!(result["results"][0]["title"], "new");
    assert_eq!(new_fixture.requests.lock().unwrap().len(), 1);
    repl.shutdown().await;
    old_server.abort();
    new_server.abort();
}

#[tokio::test]
async fn exposed_parent_and_research_profile_keep_independent_web_budgets() {
    let root = tempfile::tempdir().unwrap();
    let (old_base, old_fixture, old_server) = serve("old", false, false).await;
    let (new_base, new_fixture, new_server) = serve("new", false, false).await;
    let live = Arc::new(
        LiveProviderConfig::new(configuration(root.path(), &old_base, "old", "")).unwrap(),
    );
    let web = web_configuration(&live);
    let store = Store::in_memory().unwrap();
    let repl = Arc::new(
        CodeMode::new(
            root.path().into(),
            Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap()),
        )
        .unwrap(),
    );
    let mut tools = ToolRegistry::new();
    register_fetch_search_tools_with_store_live(&mut tools, &web, &store, live.clone()).unwrap();
    let profile = research_profile_with_tools_live(
        store.clone(),
        Arc::new(ScriptedProvider::new("unused")),
        &tools,
        &repl,
        &HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            ..Default::default()
        },
        &web,
        live.clone(),
    )
    .unwrap();
    let parent = store.create_session("parent").unwrap().id;
    let context = || ToolContext {
        session_id: parent.clone(),
        run_id: "fixture".into(),
        invocation_id: uuid::Uuid::new_v4().to_string(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    };
    let tool = tools.get_host("web_search").unwrap();
    for index in 0..3 {
        tool.invoke(context(), json!({"query":format!("parent miss {index}")}))
            .await
            .unwrap();
    }
    apply(&live, configuration(root.path(), &new_base, "new", ""));
    let child = store.create_session("research").unwrap().id;
    let result = profile
        .harness
        .prompt(
            &child,
            "fixture research",
            RunOptions {
                script: Some(
                    "return await tools.web_search({query:'independent child miss'});".into(),
                ),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result.status, "completed", "{:?}", result.error);
    let result: Value = serde_json::from_str(result.text.as_deref().unwrap()).unwrap();
    assert_eq!(result["results"][0]["title"], "new");
    let error = tool
        .invoke(context(), json!({"query":"parent budget still exhausted"}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("search budget exhausted"));
    assert_eq!(old_fixture.requests.lock().unwrap().len(), 3);
    assert_eq!(new_fixture.requests.lock().unwrap().len(), 1);
    repl.shutdown().await;
    old_server.abort();
    new_server.abort();
}

#[path = "live_roles_net/crawl_token_snapshot.rs"]
mod crawl_token_snapshot;
