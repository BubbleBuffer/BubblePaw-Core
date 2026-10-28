//! Live generations through synthetic loopback transports. The test reexecutes
//! itself with a private fixture environment instead of mutating process env.
#[path = "support/chat_messages.rs"]
mod chat_messages;
use axum::{
    Router,
    extract::State,
    http::header,
    response::{IntoResponse, Response},
    routing::post,
};
use bubblepaw_core::{
    CancellationToken, ModelRequest, Provider, ProviderError, ProviderErrorKind, Role,
};
use bubblepaw_runner::{
    config::{EffectiveConfig, LoadOptions},
    provider::LiveProviderConfig,
};
use chat_messages::uuid_message;
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::Notify;

const KEY_ENV: &str = "BP_PROVIDER_RELOAD_FIXTURE_KEY";
const CHILD_ENV: &str = "BP_PROVIDER_RELOAD_FIXTURE_CHILD";

fn options(path: &Path) -> LoadOptions {
    LoadOptions {
        workspace: path.parent().unwrap().to_owned(),
        home: None,
        custom_file: Some(path.to_owned()),
        // Execution credentials now come from this invocation snapshot, not
        // process-global lookups performed later by the provider host.
        env: std::env::vars()
            .filter(|(name, _)| {
                name == KEY_ENV || matches!(name.as_str(), "PATH" | "SystemRoot" | "TEMP" | "TMP")
            })
            .collect(),
        cli: vec![],
    }
}
fn load(path: &Path, text: &str) -> EffectiveConfig {
    EffectiveConfig::load_with_file(options(path), path, text).unwrap()
}
fn config(base: &str, main: &str, kind: &str, models: &[&str]) -> String {
    let models: String = models
        .iter()
        .map(|id| {
            let replay = if kind == "chat-completions" {
                "reasoning_replay='all'\n"
            } else {
                ""
            };
            format!("[[providers.fixture.models]]\nid='{id}'\n{replay}")
        })
        .collect();
    format!(
        "[model]\nmain='{main}'\nprovider='fixture'\n[providers.fixture]\nkind='{kind}'\nbase_url='{base}'\nkey_env='{KEY_ENV}'\n{models}"
    )
}
fn request(model: &str) -> ModelRequest {
    ModelRequest {
        model: model.into(),
        messages: vec![uuid_message(
            "synthetic-session",
            Role::User,
            "synthetic task",
        )],
        ..Default::default()
    }
}
async fn generate(
    provider: &impl Provider,
    request: ModelRequest,
) -> anyhow::Result<bubblepaw_core::ModelResponse> {
    provider
        .generate(request, Arc::new(|_| {}), CancellationToken::new())
        .await
}
fn fatal(error: &anyhow::Error, fragment: &str) {
    assert_eq!(
        ProviderError::of(error).map(|error| &error.kind),
        Some(&ProviderErrorKind::Fatal)
    );
    assert!(
        error.to_string().contains(fragment),
        "unexpected local error: {error}"
    );
}

#[derive(Clone)]
struct Fixture {
    label: &'static str,
    messages: bool,
    requests: Arc<Mutex<Vec<Value>>>,
    raw_requests: Arc<Mutex<Vec<Vec<u8>>>>,
    block: Arc<AtomicBool>,
    started: Arc<Notify>,
    release: Arc<Notify>,
}
async fn handle(State(state): State<Fixture>, body: String) -> Response {
    state
        .raw_requests
        .lock()
        .unwrap()
        .push(body.as_bytes().to_vec());
    let body: Value = serde_json::from_str(&body).unwrap();
    state.requests.lock().unwrap().push(body.clone());
    if state.block.swap(false, Ordering::SeqCst) {
        state.started.notify_one();
        state.release.notified().await;
    }
    let events = if state.messages {
        vec![
            json!({"type":"message_start","message":{"id":"synthetic","type":"message","role":"assistant","model":body["model"],"content":[],"usage":{"input_tokens":1,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"synthetic plan","signature":"synthetic-signature"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call-1","name":"echo","input":{}}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":1}}),
            json!({"type":"message_stop"}),
        ]
    } else {
        vec![
            json!({"id":"synthetic","choices":[{"index":0,"delta":{"content":state.label,"reasoning_content":"synthetic-reasoning"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}),
        ]
    };
    let mut stream: String = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect();
    if !state.messages {
        stream.push_str("data: [DONE]\n\n");
    }
    ([(header::CONTENT_TYPE, "text/event-stream")], stream).into_response()
}
async fn start(
    label: &'static str,
    messages: bool,
) -> (String, Fixture, tokio::task::JoinHandle<()>) {
    let state = Fixture {
        label,
        messages,
        requests: Arc::default(),
        raw_requests: Arc::default(),
        block: Arc::default(),
        started: Arc::default(),
        release: Arc::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/chat/completions", post(handle))
        .route("/messages", post(handle))
        .with_state(state.clone());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, state, task)
}

#[test]
fn live_provider_reload_local_transports() {
    if std::env::var_os(CHILD_ENV).is_none() {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "live_provider_reload_local_transports",
                "--nocapture",
            ])
            .env_clear()
            .env(CHILD_ENV, "1")
            .env(KEY_ENV, "synthetic-local-fixture");
        // Winsock needs SystemRoot to initialize even loopback connections.
        #[cfg(windows)]
        command.env(
            "SystemRoot",
            std::env::var_os("SystemRoot").expect("Windows SystemRoot is missing"),
        );
        #[cfg(windows)]
        for name in ["TEMP", "TMP"] {
            if let Some(path) = std::env::var_os(name) {
                command.env(name, path);
            }
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "fixture failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.toml");
            let (first_base, first, first_task) = start("first", false).await;
            let (second_base, second, second_task) = start("second", false).await;
            let initial = config(
                &first_base,
                "fixture/model-a",
                "chat-completions",
                &["model-a"],
            );
            std::fs::write(&path, &initial).unwrap();
            let live = Arc::new(LiveProviderConfig::new(load(&path, &initial)).unwrap());
            let original = live.snapshot();
            let initial_reply = generate(&*live, request("fixture/model-a")).await.unwrap();
            assert_eq!(initial_reply.text, "first");

            // A request dispatched before publication retains its client, model and
            // endpoint until completion, even while the next request uses new ones.
            first.block.store(true, Ordering::SeqCst);
            let running = {
                let live = live.clone();
                tokio::spawn(async move { generate(&*live, request("fixture/model-a")).await })
            };
            tokio::time::timeout(std::time::Duration::from_secs(5), first.started.notified())
                .await
                .unwrap();
            let changed = config(
                &second_base,
                "fixture/model-b",
                "chat-completions",
                &["model-b"],
            );
            {
                let _update = live.lock_update().unwrap();
                let candidate = live.prepare(load(&path, &changed)).unwrap();
                live.apply_prepared(candidate);
            }
            assert_eq!(live.main_model(), "fixture/model-b");
            assert_eq!(
                generate(&*live, request("fixture/model-b"))
                    .await
                    .unwrap()
                    .text,
                "second"
            );
            first.release.notify_one();
            assert_eq!(running.await.unwrap().unwrap().text, "first");
            assert_eq!(
                first.requests.lock().unwrap().last().unwrap()["model"],
                "model-a"
            );
            assert_eq!(
                second.requests.lock().unwrap().last().unwrap()["model"],
                "model-b"
            );
            assert_eq!(
                generate(&*original.registry, request("fixture/model-a"))
                    .await
                    .unwrap()
                    .text,
                "first"
            );

            // Invalid startup policy/client candidates publish nothing.
            let serving = live.snapshot();
            let invalid = config(
                "http://example.com",
                "fixture/model-b",
                "chat-completions",
                &["model-b"],
            );
            assert!(live.prepare(load(&path, &invalid)).is_err());
            assert!(Arc::ptr_eq(&serving, &live.snapshot()));
            let invalid = changed.replace(KEY_ENV, "BP_UNKNOWN_FIXTURE_SECRET");
            assert!(live.prepare(load(&path, &invalid)).is_err());
            assert!(Arc::ptr_eq(&serving, &live.snapshot()));

            // Removed configured IDs remain tombstoned through unrelated reloads;
            // legacy arbitrary bare IDs and an explicit re-add still work.
            fatal(
                &generate(&*live, request("fixture/model-a"))
                    .await
                    .unwrap_err(),
                "removed",
            );
            fatal(
                &generate(&*live, request("removed/model-b"))
                    .await
                    .unwrap_err(),
                "unavailable",
            );
            let removed_main = config(
                &second_base,
                "fixture/model-a",
                "chat-completions",
                &["model-b"],
            );
            assert!(live.prepare(load(&path, &removed_main)).is_err());
            assert_eq!(
                generate(&*live, request("legacy-bare-model"))
                    .await
                    .unwrap()
                    .text,
                "second"
            );
            assert!(live.prepare(load(&path, &initial)).is_ok());
            {
                let _update = live.lock_update().unwrap();
                live.apply_prepared(live.prepare(load(&path, &changed)).unwrap());
            }
            fatal(
                &generate(&*live, request("fixture/model-a"))
                    .await
                    .unwrap_err(),
                "removed",
            );

            // Same adapter and model at another endpoint cannot receive old opaque
            // reasoning. The source history remains unchanged.
            let same_model = config(
                &second_base,
                "fixture/model-a",
                "chat-completions",
                &["model-a"],
            );
            {
                let _update = live.lock_update().unwrap();
                live.apply_prepared(live.prepare(load(&path, &same_model)).unwrap());
            }
            let mut assistant =
                uuid_message("synthetic-session", Role::Assistant, initial_reply.text);
            assistant.provider_state = initial_reply.provider_state;
            let preserved = assistant.provider_state.clone();
            let mut replay = request("fixture/model-a");
            replay.messages.extend([
                assistant.clone(),
                uuid_message("synthetic-session", Role::User, "continue"),
            ]);
            generate(&*live, replay).await.unwrap();
            assert!(
                second.requests.lock().unwrap().last().unwrap()["messages"][1]
                    .get("reasoning_content")
                    .is_none()
            );
            assert_eq!(assistant.provider_state, preserved);

            // A distinct provider alias at the original endpoint is also foreign.
            let other_route = config(
                &first_base,
                "fixture/model-a",
                "chat-completions",
                &["model-a"],
            )
            .replace("providers.fixture", "providers.other")
            .replace("fixture/model-a", "other/model-a")
            .replace("provider='fixture'", "provider='other'");
            {
                let _update = live.lock_update().unwrap();
                live.apply_prepared(live.prepare(load(&path, &other_route)).unwrap());
            }
            let mut replay = request("other/model-a");
            replay.messages.extend([
                assistant,
                uuid_message("synthetic-session", Role::User, "continue"),
            ]);
            generate(&*live, replay).await.unwrap();
            assert!(
                first.requests.lock().unwrap().last().unwrap()["messages"][1]
                    .get("reasoning_content")
                    .is_none()
            );
            fatal(
                &generate(&*live, request("fixture/model-a"))
                    .await
                    .unwrap_err(),
                "unavailable",
            );

            // Reload uses the startup layering inputs, and parse/apply failure
            // retains both active registry and configuration generation.
            std::fs::write(&path, &changed).unwrap();
            live.reload().unwrap();
            assert_eq!(live.main_model(), "fixture/model-b");
            let serving = live.snapshot();
            std::fs::write(&path, "invalid = [").unwrap();
            assert!(live.reload().is_err());
            assert!(Arc::ptr_eq(&serving, &live.snapshot()));

            first_task.abort();
            second_task.abort();
            signed_messages_endpoint_guard(&path).await;
        });
}

async fn signed_messages_endpoint_guard(path: &Path) {
    let (first_base, first, first_task) = start("first", true).await;
    let (second_base, second, second_task) = start("second", true).await;
    let initial = config(
        &first_base,
        "fixture/signed-model",
        "anthropic-messages",
        &[],
    );
    let live = LiveProviderConfig::new(load(path, &initial)).unwrap();
    let mut first_request = request("fixture/signed-model");
    first_request.tools.push(bubblepaw_core::ToolDefinition {
        name: "echo".into(),
        description: "fixture".into(),
        input_schema: json!({"type":"object"}),
        read_only: true,
        approval: Default::default(),
    });
    let reply = generate(&live, first_request.clone()).await.unwrap();
    assert_eq!(reply.tool_calls.len(), 1);
    let mut assistant = uuid_message("synthetic-session", Role::Assistant, reply.text);
    assistant.tool_calls = reply.tool_calls;
    assistant.provider_state = reply.provider_state;
    let mut result = uuid_message("synthetic-session", Role::Tool, "synthetic result");
    result.tool_call_id = Some("call-1".into());
    let mut followup = request("fixture/signed-model");
    followup.tools = first_request.tools;
    followup.messages.extend([assistant, result]);
    // Same endpoint can replay the original signed block.
    generate(&live, followup.clone()).await.unwrap();
    assert_eq!(first.requests.lock().unwrap().len(), 2);
    let mut legacy = followup.clone();
    legacy.messages[1]
        .provider_state
        .as_mut()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("_bubblepaw_route_origin");
    // A plain upgrade retains pre-route-tag signed history. Reconstruct the
    // host with unchanged config and assert its request bytes, including thinking
    // and signature, are exactly those of the native continuation above.
    let preserved = legacy.messages[1].provider_state.clone();
    let live = LiveProviderConfig::new(load(path, &initial)).unwrap();
    generate(&live, legacy.clone()).await.unwrap();
    assert_eq!(first.requests.lock().unwrap().len(), 3);
    {
        let requests = first.raw_requests.lock().unwrap();
        assert_eq!(
            requests[1], requests[2],
            "upgrade changed signed request bytes"
        );
    }
    assert_eq!(legacy.messages[1].provider_state, preserved);
    // Missing route provenance does not bypass native model/kind checks.
    let mut foreign_model = legacy.clone();
    foreign_model.model = "fixture/another-model".into();
    fatal(
        &generate(&live, foreign_model).await.unwrap_err(),
        "compatible replay state",
    );
    let mut foreign_kind = legacy;
    foreign_kind.messages[1].provider_state.as_mut().unwrap()
        [bubblepaw_core::PROVIDER_STATE_ORIGIN_FIELD]["provider_kind"] = json!("chat-completions");
    fatal(
        &generate(&live, foreign_kind).await.unwrap_err(),
        "compatible replay state",
    );
    assert_eq!(first.requests.lock().unwrap().len(), 3);
    let changed = config(
        &second_base,
        "fixture/signed-model",
        "anthropic-messages",
        &[],
    );
    // Recorded endpoint provenance stays authoritative after an explicit reload.
    std::fs::write(path, &changed).unwrap();
    live.reload().unwrap();
    fatal(
        &generate(&live, followup).await.unwrap_err(),
        "compatible replay state",
    );
    assert!(second.requests.lock().unwrap().is_empty());
    first_task.abort();
    second_task.abort();
}

#[test]
fn exec_applies_live_while_storage_cache_and_disabled_urls_are_retained() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let initial = "[model]\nmain='demo/test'\n[providers.opencode]\nbase_url=''\n[providers.opencode-messages]\nbase_url=''\n[storage]\narchive_after_days=5\n[web]\ncache_ttl_secs=7\n[exec]\nsync_timeout_secs=5\n";
    let live = LiveProviderConfig::new(load(&path, initial)).unwrap();
    let changed = initial.replace("sync_timeout_secs=5", "sync_timeout_secs=25");
    let exec_only = load(&path, &changed);
    assert!(!exec_only.restart_needed(live.startup_config()));
    assert!(!exec_only.same_provider_config(live.startup_config()));
    let changed = changed
        .replace("archive_after_days=5", "archive_after_days=9")
        .replace("cache_ttl_secs=7", "cache_ttl_secs=17");
    let effective = load(&path, &changed);
    assert!(effective.restart_needed(live.startup_config()));
    assert!(!effective.same_provider_config(live.startup_config()));
    let _update = live.lock_update().unwrap();
    let applied = live.apply_prepared(live.prepare(effective).unwrap());
    assert_eq!(applied.settings.exec.sync_timeout_secs, 25);
    assert_eq!(applied.settings.storage.archive_after_days, 5);
    assert_eq!(applied.settings.web.cache_ttl_secs, 7);
    assert_eq!(
        applied.settings.providers["opencode"].base_url.as_deref(),
        Some("")
    );
    assert_eq!(
        applied
            .effective
            .settings()
            .unwrap()
            .storage
            .archive_after_days,
        9
    );
    assert_eq!(applied.effective.settings().unwrap().web.cache_ttl_secs, 17);
    assert_eq!(
        applied.effective.settings().unwrap().exec.sync_timeout_secs,
        25
    );
}

#[path = "provider_reload/extension_client.rs"]
mod extension_client;
