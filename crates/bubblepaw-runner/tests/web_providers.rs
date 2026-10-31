#![cfg(feature = "web")]

//! Provider setup acceptance uses only ephemeral files and loopback HTTP fixtures.
//! No provider generation endpoint or public service is contacted.
use axum::{
    Json, Router, extract::Request, http::StatusCode, response::IntoResponse, routing::get,
};
use bubblepaw_runner::{
    config::{EffectiveConfig, LoadOptions, Override},
    web_providers::{ProviderSetup, router},
};
use reqwest::{Client, Response};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tempfile::TempDir;
use tokio::{net::TcpListener, task::JoinHandle};

struct Server {
    url: String,
    task: Option<JoinHandle<()>>,
}

impl Server {
    async fn start(app: Router) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            url,
            task: Some(task),
        }
    }

    async fn stop(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Also terminate a fixture if an assertion unwinds before explicit cleanup.
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

struct Files {
    _temp: TempDir,
    home: PathBuf,
    workspace: PathBuf,
    custom: PathBuf,
}

impl Files {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let workspace = temp.path().join("workspace");
        let custom = temp.path().join("custom.toml");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        Self {
            _temp: temp,
            home,
            workspace,
            custom,
        }
    }

    fn options(&self, custom: bool) -> LoadOptions {
        LoadOptions {
            workspace: self.workspace.clone(),
            home: Some(self.home.clone()),
            custom_file: custom.then(|| self.custom.clone()),
            env: Vec::new(),
            cli: Vec::new(),
        }
    }

    async fn setup(&self, options: LoadOptions) -> Server {
        let launched = EffectiveConfig::load(options.clone()).unwrap();
        Server::start(router(ProviderSetup::new(options, &launched).unwrap())).await
    }

    fn user(&self) -> PathBuf {
        self.home.join(".bubblepaw/config.toml")
    }
    fn project(&self) -> PathBuf {
        self.workspace.join(".bubblepaw/config.toml")
    }
}

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap()
}

async fn payload(response: Response) -> Value {
    assert!(
        response.status().is_success(),
        "status {}",
        response.status()
    );
    response.json().await.unwrap()
}

async fn listing(client: &Client, server: &Server) -> Value {
    payload(
        client
            .get(format!("{}/api/providers", server.url))
            .send()
            .await
            .unwrap(),
    )
    .await
}

fn provider<'a>(listing: &'a Value, name: &str) -> &'a Value {
    listing["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == name)
        .unwrap()
}

fn draft(kind: &str, base: &str) -> Value {
    json!({"kind": kind, "base_url": base, "key_env": "BP_WEB_MISSING_CREDENTIAL_75462_KEY", "models": [{"id": "model-one", "reasoning_replay": "none"}]})
}

#[tokio::test]
async fn saving_omitted_replay_policy_preserves_provider_defaults() {
    let files = Files::new();
    write(&files.custom, "");
    let server = files.setup(files.options(true)).await;
    let client = client();
    for kind in ["chat-completions", "responses", "anthropic-messages"] {
        let mut config = draft(kind, "http://127.0.0.1:1/v1");
        config["models"][0]
            .as_object_mut()
            .unwrap()
            .remove("reasoning_replay");
        let saved = payload(
            client
                .put(format!("{}/api/providers/local", server.url))
                .json(&config)
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            provider(&saved, "local")["models"][0]["reasoning_replay"],
            if kind == "chat-completions" {
                "tool_loop"
            } else {
                "none"
            }
        );
        let loaded = EffectiveConfig::load(files.options(true))
            .unwrap()
            .settings()
            .unwrap();
        assert!(loaded.providers["local"].reasoning_replay().is_empty());
        let doc: toml::Value =
            toml::from_str(&std::fs::read_to_string(&files.custom).unwrap()).unwrap();
        assert!(
            doc["providers"]["local"]["models"][0]
                .get("reasoning_replay")
                .is_none()
        );
    }
    server.stop().await;
}

async fn error(response: Response, status: StatusCode, forbidden: &str) {
    assert_eq!(response.status(), status);
    let text = response.text().await.unwrap();
    assert!(
        !text.contains(forbidden),
        "error reflected request or credentials"
    );
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body.as_object().unwrap().len(), 1);
    assert!(body["error"].is_string());
}

async fn policy_error(response: Response, expected: &str) {
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        json!({"error":expected})
    );
}

#[tokio::test]
async fn plaintext_remote_urls_are_rejected_for_save_and_both_test_modes() {
    let files = Files::new();
    let original = "# policy fixture\n[providers.local]\nkind = 'responses'\nbase_url = 'http://198.51.100.1:1/v1'\nkey_env = 'MY_SERVICE_TOKEN'\n";
    write(&files.custom, original);
    let server = files.setup(files.options(true)).await;
    let client = client();
    let expected =
        "base_url must use https unless it points at this machine (localhost/127.0.0.1/::1)";
    for base in [
        "http://198.51.100.1:1/v1",
        "http://service.example/v1",
        "http://localhost.example/v1",
        "http://192.168.1.2/v1",
    ] {
        let config = draft("responses", base);
        policy_error(
            client
                .put(format!("{}/api/providers/local", server.url))
                .json(&config)
                .send()
                .await
                .unwrap(),
            expected,
        )
        .await;
        policy_error(
            client
                .post(format!("{}/api/providers/local/test", server.url))
                .json(&json!({"draft":config}))
                .send()
                .await
                .unwrap(),
            expected,
        )
        .await;
    }
    policy_error(
        client
            .post(format!("{}/api/providers/local/test", server.url))
            .send()
            .await
            .unwrap(),
        expected,
    )
    .await;
    assert_eq!(std::fs::read_to_string(&files.custom).unwrap(), original);
    assert_eq!(listing(&client, &server).await["restart_needed"], false);
    server.stop().await;
}

#[tokio::test]
async fn loopback_urls_are_accepted_for_save_and_both_test_modes() {
    let files = Files::new();
    write(&files.custom, "# loopback policy fixture\n");
    let mock = Server::start(Router::new().route(
        "/models",
        get(|| async { Json(json!({"data":[{"id":"model-one"}]})) }),
    ))
    .await;
    let server = files.setup(files.options(true)).await;
    let client = client();
    for base in [mock.url.clone(), mock.url.replace("127.0.0.1", "localhost")] {
        let config = draft("responses", &base);
        payload(
            client
                .put(format!("{}/api/providers/local", server.url))
                .json(&config)
                .send()
                .await
                .unwrap(),
        )
        .await;
        for body in [json!({"draft":config}), json!({})] {
            let result = payload(
                client
                    .post(format!("{}/api/providers/local/test", server.url))
                    .json(&body)
                    .send()
                    .await
                    .unwrap(),
            )
            .await;
            assert_eq!(result["reachable"], true);
            assert_eq!(result["models_listed"], json!(["model-one"]));
        }
    }
    server.stop().await;
    mock.stop().await;
}

#[tokio::test]
async fn key_variable_policy_uses_a_fresh_file_allowlist_for_every_request() {
    let files = Files::new();
    let original = "# key policy fixture\n[providers.local]\nkind = 'scripted'\nkey_env = 'MY_SERVICE_CREDENTIAL'\n";
    write(&files.custom, original);
    let server = files.setup(files.options(true)).await;
    let client = client();
    assert_eq!(
        listing(&client, &server).await["allowed_key_envs"],
        json!([])
    );
    for name in ["HOME", "MY_SERVICE_CREDENTIAL"] {
        let config = json!({"kind":"scripted", "key_env":name, "models":[]});
        let expected = format!(
            "key_env {name} is not an allowed key variable; names must end in _API_KEY, _KEY or _TOKEN, or be listed in [web] provider_key_envs"
        );
        policy_error(
            client
                .put(format!("{}/api/providers/local", server.url))
                .json(&config)
                .send()
                .await
                .unwrap(),
            &expected,
        )
        .await;
        policy_error(
            client
                .post(format!("{}/api/providers/local/test", server.url))
                .json(&json!({"draft":config}))
                .send()
                .await
                .unwrap(),
            &expected,
        )
        .await;
        write(
            &files.custom,
            &format!("[providers.local]\nkind = 'scripted'\nkey_env = '{name}'\n"),
        );
        policy_error(
            client
                .post(format!("{}/api/providers/local/test", server.url))
                .send()
                .await
                .unwrap(),
            &expected,
        )
        .await;
    }
    for name in ["MY_SERVICE_API_KEY", "MY_SERVICE_KEY", "MY_SERVICE_TOKEN"] {
        let config = json!({"kind":"scripted", "key_env":name, "models":[]});
        payload(
            client
                .put(format!("{}/api/providers/local", server.url))
                .json(&config)
                .send()
                .await
                .unwrap(),
        )
        .await;
        for body in [json!({"draft":config}), json!({})] {
            let result = payload(
                client
                    .post(format!("{}/api/providers/local/test", server.url))
                    .json(&body)
                    .send()
                    .await
                    .unwrap(),
            )
            .await;
            assert_eq!(result["config"]["ok"], true);
        }
    }
    let config = json!({"kind":"scripted", "key_env":"MY_SERVICE_CREDENTIAL", "models":[]});
    write(
        &files.custom,
        &format!("[web]\nprovider_key_envs = ['MY_SERVICE_CREDENTIAL']\n{original}"),
    );
    assert_eq!(
        listing(&client, &server).await["allowed_key_envs"],
        json!(["MY_SERVICE_CREDENTIAL"])
    );
    let saved = payload(
        client
            .put(format!("{}/api/providers/local", server.url))
            .json(&config)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(saved["allowed_key_envs"], json!(["MY_SERVICE_CREDENTIAL"]));
    for body in [json!({"draft":config.clone()}), json!({})] {
        let result = payload(
            client
                .post(format!("{}/api/providers/local/test", server.url))
                .json(&body)
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(result["config"]["ok"], true);
    }
    // Removing the allowance applies immediately to Save and saved/draft Test.
    write(&files.custom, original);
    assert_eq!(
        listing(&client, &server).await["allowed_key_envs"],
        json!([])
    );
    let expected = "key_env MY_SERVICE_CREDENTIAL is not an allowed key variable; names must end in _API_KEY, _KEY or _TOKEN, or be listed in [web] provider_key_envs";
    policy_error(
        client
            .put(format!("{}/api/providers/local", server.url))
            .json(&config)
            .send()
            .await
            .unwrap(),
        expected,
    )
    .await;
    for body in [json!({"draft":config}), json!({})] {
        policy_error(
            client
                .post(format!("{}/api/providers/local/test", server.url))
                .json(&body)
                .send()
                .await
                .unwrap(),
            expected,
        )
        .await;
    }
    assert_eq!(std::fs::read_to_string(&files.custom).unwrap(), original);
    server.stop().await;
}

#[tokio::test]
async fn listing_tracks_sources_target_and_launched_config_without_credentials() {
    let files = Files::new();
    write(
        &files.custom,
        "# setup fixture\n[model]\nmain = 'demo/fixture'\n[providers.local]\nkind = 'responses'\nbase_url = 'http://127.0.0.1:1/v1'\nkey_env = 'BP_WEB_UNSET_74251_KEY'\nmodels = [{ id = 'model-one' }]\n",
    );
    let server = files.setup(files.options(true)).await;
    let client = client();
    let initial = listing(&client, &server).await;
    assert_eq!(
        initial["target"]["path"],
        files.custom.to_string_lossy().as_ref()
    );
    assert_eq!(initial["target"]["layer"], "custom");
    assert_eq!(initial["target"]["exists"], true);
    assert_eq!(initial["restart_needed"], false);
    assert_eq!(initial["main"], initial["launched_main"]);
    let local = provider(&initial, "local");
    assert_eq!(local["kind"], "responses");
    assert_eq!(local["key_env"], "BP_WEB_UNSET_74251_KEY");
    assert_eq!(local["key_present"], false);
    assert_eq!(local["editable"], true);
    assert_eq!(local["deletable"], true);
    assert!(
        local["source"]
            .as_str()
            .unwrap()
            .contains(files.custom.to_string_lossy().as_ref())
    );
    assert!(local.get("auth_file").is_some());
    assert!(local.get("reasoning").is_some());
    assert_eq!(local["models"][0]["id"], "model-one");
    assert!(
        initial["kinds"]
            .as_array()
            .unwrap()
            .iter()
            .any(|kind| kind == "codex")
    );
    assert_eq!(provider(&initial, "demo")["deletable"], false);

    // Every request rereads disk; an external edit changes the serving config only.
    write(
        &files.custom,
        "[model]\nmain = 'demo/fixture'\n[providers.local]\nkind = 'responses'\nbase_url = 'http://127.0.0.1:2/v1'\nkey_env = 'BP_WEB_UNSET_74251_KEY'\n",
    );
    let changed = listing(&client, &server).await;
    assert_eq!(
        provider(&changed, "local")["base_url"],
        "http://127.0.0.1:2/v1"
    );
    assert_eq!(changed["restart_needed"], true);
    assert_eq!(changed["launched_main"], initial["launched_main"]);
    server.stop().await;
}

#[tokio::test]
async fn relative_custom_and_auth_paths_do_not_report_a_startup_restart() {
    let cwd = std::env::current_dir().unwrap();
    // A sibling fixture stays on the current volume on both Windows and Linux.
    let temporary = tempfile::tempdir_in(cwd.parent().unwrap()).unwrap();
    let target = temporary.path().join("config.toml");
    let auth_path = temporary.path().join("fixture-auth.json");
    write(
        &target,
        "[providers.local]\nkind = 'scripted'\nauth_file = 'fixture-auth.json'\n",
    );
    write(&auth_path, "synthetic auth-path fixture");
    let relative_target = PathBuf::from("..")
        .join(temporary.path().file_name().unwrap())
        .join("config.toml");
    assert!(relative_target.is_relative());
    let options = LoadOptions {
        workspace: cwd,
        home: Some(temporary.path().join("home")),
        custom_file: Some(relative_target),
        env: Vec::new(),
        cli: Vec::new(),
    };
    let launched = EffectiveConfig::load(options.clone()).unwrap();
    assert!(
        launched.settings().unwrap().providers["local"]
            .auth_file
            .as_ref()
            .unwrap()
            .is_relative()
    );
    let server = Server::start(router(ProviderSetup::new(options, &launched).unwrap())).await;
    let client = client();
    let listed = listing(&client, &server).await;
    assert_eq!(listed["restart_needed"], false);
    let reported_auth = PathBuf::from(provider(&listed, "local")["auth_file"].as_str().unwrap());
    assert!(reported_auth.is_absolute());
    assert_eq!(
        reported_auth.canonicalize().unwrap(),
        auth_path.canonicalize().unwrap()
    );
    let reported_target = PathBuf::from(listed["target"]["path"].as_str().unwrap());
    assert_eq!(
        reported_target.canonicalize().unwrap(),
        target.canonicalize().unwrap()
    );
    server.stop().await;
}

#[tokio::test]
async fn clearing_default_scalars_is_rejected_with_a_specific_error() {
    let files = Files::new();
    let original = "# keep this header\n[providers.codex]\nbase_url = 'http://127.0.0.1:1'\n[providers.opencode]\nkey_env = 'BP_WEB_OVERRIDE_82471_KEY'\n[storage]\narchive_after_days = 23 # keep storage comment\n";
    write(&files.custom, original);
    let server = files.setup(files.options(true)).await;
    let client = client();
    let list = listing(&client, &server).await;
    assert!(
        provider(&list, "codex")["inherited_fields"]
            .as_array()
            .unwrap()
            .contains(&json!("base_url"))
    );
    assert!(
        provider(&list, "opencode")["inherited_fields"]
            .as_array()
            .unwrap()
            .contains(&json!("key_env"))
    );
    for (name, field) in [("codex", "base_url"), ("opencode", "key_env")] {
        for empty in [false, true] {
            let mut replacement = json!({"kind":"scripted", "models":[], "reasoning":{}});
            if name == "opencode" {
                replacement["base_url"] = json!("http://127.0.0.1:1/v1");
            }
            if empty {
                replacement[field] = json!("");
            }
            let response = client
                .put(format!("{}/api/providers/{name}", server.url))
                .json(&replacement)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let body: Value = response.json().await.unwrap();
            assert_eq!(body.as_object().unwrap().len(), 1);
            assert_eq!(
                body["error"],
                format!(
                    "{field} is inherited from defaults and can't be cleared here; set a value instead"
                )
            );
            assert_eq!(std::fs::read_to_string(&files.custom).unwrap(), original);
            assert_eq!(listing(&client, &server).await["restart_needed"], false);
        }
    }
    server.stop().await;
}

#[tokio::test]
async fn omitted_and_empty_scalars_remove_target_only_fields() {
    let files = Files::new();
    let original = "# keep this header\n[providers.local]\nkind = 'scripted'\nbase_url = 'http://127.0.0.1:1/v1'\nkey_env = 'BP_WEB_TARGET_ONLY_39171_KEY'\nauth_file = 'fixture-auth.json'\n[storage]\narchive_after_days = 23 # keep storage comment\n";
    write(&files.custom, original);
    let server = files.setup(files.options(true)).await;
    let client = client();
    assert_eq!(
        provider(&listing(&client, &server).await, "local")["inherited_fields"],
        json!([])
    );
    for empty in [false, true] {
        write(&files.custom, original);
        let mut replacement = json!({"kind":"scripted", "models":[], "reasoning":{}});
        if empty {
            replacement["base_url"] = json!("");
            replacement["key_env"] = json!("");
            replacement["auth_file"] = json!("");
        }
        let saved = payload(
            client
                .put(format!("{}/api/providers/local", server.url))
                .json(&replacement)
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(saved["restart_needed"], true);
        let local = provider(&saved, "local");
        for field in ["base_url", "key_env", "auth_file"] {
            assert!(local[field].is_null());
        }
        assert_eq!(local["inherited_fields"], json!([]));
        let text = std::fs::read_to_string(&files.custom).unwrap();
        assert!(text.contains("# keep this header"));
        assert!(text.contains("# keep storage comment"));
        let doc: toml::Value = toml::from_str(&text).unwrap();
        for field in ["base_url", "key_env", "auth_file"] {
            assert!(doc["providers"]["local"].get(field).is_none());
        }
        assert_eq!(doc["storage"]["archive_after_days"].as_integer(), Some(23));
        let effective = EffectiveConfig::load(files.options(true))
            .unwrap()
            .settings()
            .unwrap();
        let local = &effective.providers["local"];
        assert!(local.base_url.is_none());
        assert!(local.key_env.is_none());
        assert!(local.auth_file.is_none());
    }
    let mut chat = draft("chat-completions", "http://127.0.0.1:8/v1");
    chat["models"][0]["reasoning_replay"] = json!("tool_loop");
    let saved = payload(
        client
            .put(format!("{}/api/providers/local", server.url))
            .json(&chat)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        provider(&saved, "local")["models"][0]["reasoning_replay"],
        "tool_loop"
    );
    assert_eq!(
        provider(&listing(&client, &server).await, "local")["models"][0]["reasoning_replay"],
        "tool_loop"
    );
    server.stop().await;
}

#[tokio::test]
async fn saving_complete_http_provider_preserves_its_inline_comments() {
    let files = Files::new();
    let key_name = format!("BP_WEB_SAVE_{}_KEY", uuid::Uuid::new_v4().simple());
    let secret = "PRIVATE_POSITIVE_SAVE_FIXTURE_68241";
    let original = format!(
        "# preserve setup header\n[providers.local]\nkind = 'responses' # preserve kind comment\nbase_url = 'http://127.0.0.1:1/v1' # preserve base comment\nkey_env = '{key_name}' # preserve key comment\nauth_file = 'old-auth.json'\n[storage]\narchive_after_days = 23 # preserve unrelated storage comment\n"
    );
    write(&files.custom, &original);
    let auth_path = files.custom.parent().unwrap().join("saved-auth.json");
    write(&auth_path, secret);
    let mut options = files.options(true);
    // Supply a synthetic credential to the loader without process-global mutation.
    options.env.push((key_name.clone(), secret.into()));
    let server = files.setup(options).await;
    let client = client();
    let config = json!({
        "kind":"responses",
        "base_url":"http://127.0.0.1:2/v1",
        "key_env":key_name,
        "auth_file":"saved-auth.json",
        "reasoning":{},
        "models":[{"id":"model-one", "reasoning_replay":"none"}],
    });
    let saved = payload(
        client
            .put(format!("{}/api/providers/local", server.url))
            .json(&config)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(saved["restart_needed"], true);
    let local = provider(&saved, "local");
    assert_eq!(local["kind"], "responses");
    assert_eq!(local["base_url"], "http://127.0.0.1:2/v1");
    assert_eq!(local["key_env"], key_name);
    assert_eq!(local["auth_file"], auth_path.to_string_lossy().as_ref());
    assert_eq!(local["models"][0]["id"], "model-one");
    assert!(!saved.to_string().contains(secret));
    let text = std::fs::read_to_string(&files.custom).unwrap();
    for comment in [
        "# preserve setup header",
        "# preserve kind comment",
        "# preserve base comment",
        "# preserve key comment",
        "# preserve unrelated storage comment",
    ] {
        assert!(text.contains(comment), "lost comment: {comment}");
    }
    assert!(!text.contains(secret));
    let document: toml::Value = toml::from_str(&text).unwrap();
    assert_eq!(
        document["storage"]["archive_after_days"].as_integer(),
        Some(23)
    );
    assert_eq!(
        provider(&listing(&client, &server).await, "local")["base_url"],
        "http://127.0.0.1:2/v1"
    );
    server.stop().await;
}

#[tokio::test]
async fn user_target_is_created_and_workspace_config_is_never_written() {
    let files = Files::new();
    let project = "# project remains byte exact\n[model]\nmain = 'demo/fixture'\n";
    write(&files.project(), project);
    let server = files.setup(files.options(false)).await;
    let client = client();
    let before = listing(&client, &server).await;
    assert_eq!(
        before["target"]["path"],
        files.user().to_string_lossy().as_ref()
    );
    assert_eq!(before["target"]["layer"], "user");
    assert_eq!(before["target"]["exists"], false);
    payload(
        client
            .put(format!("{}/api/providers/local", server.url))
            .json(&draft("responses", "http://127.0.0.1:8/v1"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(files.user().is_file());
    assert_eq!(std::fs::read_to_string(files.project()).unwrap(), project);
    assert_eq!(listing(&client, &server).await["target"]["exists"], true);
    server.stop().await;
}

#[tokio::test]
async fn environment_custom_target_replaces_user_and_project_files() {
    let files = Files::new();
    write(&files.custom, "# custom\n");
    write(&files.user(), "# ignored user\n");
    write(&files.project(), "# ignored project\n");
    let mut options = files.options(false);
    options.env.push((
        "BUBBLEPAW_CONFIG".into(),
        files.custom.to_string_lossy().into_owned(),
    ));
    let server = files.setup(options).await;
    let client = client();
    let initial = listing(&client, &server).await;
    assert_eq!(initial["main"], "gpt-6-luna");
    assert_eq!(initial["launched_main"], "gpt-6-luna");
    assert_eq!(
        listing(&client, &server).await["target"]["path"],
        files.custom.to_string_lossy().as_ref()
    );
    payload(
        client
            .put(format!("{}/api/providers/local", server.url))
            .json(&draft("responses", "http://127.0.0.1:8/v1"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(files.user()).unwrap(),
        "# ignored user\n"
    );
    assert_eq!(
        std::fs::read_to_string(files.project()).unwrap(),
        "# ignored project\n"
    );
    server.stop().await;
}

#[tokio::test]
async fn masked_saves_fail_without_mutating_the_target() {
    for layer in ["project", "env", "cli"] {
        let files = Files::new();
        let original = "# do not modify on conflict\n[providers.local]\nkind = 'responses'\nbase_url = 'http://127.0.0.1:1/v1'\nkey_env = 'BP_WEB_UNSET_74251_KEY'\n";
        write(&files.user(), original);
        let mut options = files.options(false);
        match layer {
            "project" => write(
                &files.project(),
                "[providers.local]\nbase_url = 'http://127.0.0.1:2/v1'\n",
            ),
            "env" => options.env.push((
                "BUBBLEPAW_PROVIDERS__LOCAL__BASE_URL".into(),
                "http://127.0.0.1:2/v1".into(),
            )),
            "cli" => options
                .cli
                .push(Override::set("providers.local.base_url=http://127.0.0.1:2/v1").unwrap()),
            _ => unreachable!(),
        }
        let server = files.setup(options).await;
        let client = client();
        assert_eq!(
            provider(&listing(&client, &server).await, "local")["inherited_fields"],
            json!([])
        );
        for replacement in [
            draft("responses", "http://127.0.0.1:3/v1"),
            json!({"kind":"scripted", "reasoning":{}, "models":[]}),
        ] {
            error(
                client
                    .put(format!("{}/api/providers/local", server.url))
                    .json(&replacement)
                    .send()
                    .await
                    .unwrap(),
                StatusCode::CONFLICT,
                "NEVER_REFLECT",
            )
            .await;
            assert_eq!(
                std::fs::read_to_string(files.user()).unwrap(),
                original,
                "{layer} conflict mutated target"
            );
        }
        server.stop().await;
    }
}

#[tokio::test]
async fn deletion_rereads_target_and_guards_target_main() {
    let files = Files::new();
    let original = "# deletion fixture\n[providers.local]\nkind = 'scripted'\n";
    write(&files.custom, original);
    let server = files.setup(files.options(true)).await;
    let client = client();
    let deleted = payload(
        client
            .delete(format!("{}/api/providers/local", server.url))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(
        deleted["providers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["name"] != "local")
    );
    let after = listing(&client, &server).await;
    assert!(
        after["providers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["name"] != "local")
    );
    let text = std::fs::read_to_string(&files.custom).unwrap();
    let document: toml::Value = toml::from_str(&text).unwrap();
    assert!(
        document
            .get("providers")
            .and_then(|providers| providers.get("local"))
            .is_none()
    );
    assert!(text.contains("# deletion fixture"));
    error(
        client
            .delete(format!("{}/api/providers/demo", server.url))
            .send()
            .await
            .unwrap(),
        StatusCode::NOT_FOUND,
        "NEVER_REFLECT",
    )
    .await;
    assert_eq!(std::fs::read_to_string(&files.custom).unwrap(), text);

    // Resolve both qualified and bare target model.main before deleting a provider.
    for (name, protected) in [
        ("local", format!("{original}\n[model]\nmain = 'local/fixture'\n")),
        ("local", format!("{original}\n[model]\nmain = 'fixture'\nprovider = 'local'\n")),
        ("codex", "# default main-provider guard\n[providers.codex]\nkind = 'scripted'\n[model]\nmain = 'fixture'\n".into()),
    ] {
        write(&files.custom, &protected);
        error(
            client
                .delete(format!("{}/api/providers/{name}", server.url))
                .send()
                .await
                .unwrap(),
            StatusCode::CONFLICT,
            "NEVER_REFLECT",
        )
        .await;
        assert_eq!(std::fs::read_to_string(&files.custom).unwrap(), protected);
    }
    server.stop().await;
}

#[tokio::test]
async fn main_model_validates_provider_and_blocks_deleting_its_target_provider() {
    let files = Files::new();
    write(
        &files.custom,
        "# main selection fixture\n[model]\nmain = 'demo/old'\n[providers.local]\nkind = 'responses'\nbase_url = 'http://127.0.0.1:1/v1'\nkey_env = 'BP_WEB_UNSET_74251_KEY'\nmodels = [{id='model-one'}]\n",
    );
    let server = files.setup(files.options(true)).await;
    let client = client();
    let original = std::fs::read_to_string(&files.custom).unwrap();
    error(
        client
            .put(format!("{}/api/roles/main", server.url))
            .json(&json!({"model":"missing/model-one"}))
            .send()
            .await
            .unwrap(),
        StatusCode::BAD_REQUEST,
        "NEVER_REFLECT",
    )
    .await;
    assert_eq!(std::fs::read_to_string(&files.custom).unwrap(), original);
    payload(
        client
            .put(format!("{}/api/roles/main", server.url))
            .json(&json!({"model":"local/model-one"}))
            .send()
            .await
            .unwrap(),
    )
    .await;
    let text = std::fs::read_to_string(&files.custom).unwrap();
    let config: toml::Value = toml::from_str(&text).unwrap();
    assert_eq!(config["model"]["main"].as_str(), Some("local/model-one"));
    assert!(
        config
            .get("roles")
            .and_then(|roles| roles.get("main"))
            .is_none()
    );
    let list = listing(&client, &server).await;
    assert_eq!(list["main"], "local/model-one");
    assert_ne!(list["main"], list["launched_main"]);
    assert_eq!(list["restart_needed"], true);
    assert_eq!(provider(&list, "local")["deletable"], true);
    error(
        client
            .delete(format!("{}/api/providers/local", server.url))
            .send()
            .await
            .unwrap(),
        StatusCode::CONFLICT,
        "NEVER_REFLECT",
    )
    .await;
    assert_eq!(std::fs::read_to_string(&files.custom).unwrap(), text);
    server.stop().await;
}

#[tokio::test]
async fn invalid_or_secret_bearing_payloads_are_sanitized_and_never_written() {
    let files = Files::new();
    let original = "# validation fixture\n";
    write(&files.custom, original);
    let server = files.setup(files.options(true)).await;
    let client = client();
    let marker = "PRIVATE_REJECTED_INPUT_81294";
    let invalid = [
        json!({"kind":"responses", "models":[], "api_key":marker}),
        json!({"kind":"responses", "models":[{"id":"one", "token":marker}]}),
        json!({"kind":"chat-completions", "models":[], "reasoning":{"token":marker}}),
        json!({"kind":"chat-completions", "models":[], "reasoning":{"thinking":{"type":"enabled", "api_key":marker}}}),
        json!({"kind":"responses", "models":[], "key_env":format!("BAD={marker}")}),
        json!({"kind":"responses", "models":[], "key_env":"bad name"}),
        json!({"kind":marker, "models":[]}),
        json!({"kind":"responses", "models":[], "base_url":format!("http://user:{marker}@127.0.0.1:1")}),
        json!({"kind":"responses"}),
    ];
    for body in invalid {
        error(
            client
                .put(format!("{}/api/providers/local", server.url))
                .json(&body)
                .send()
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
            marker,
        )
        .await;
        assert_eq!(std::fs::read_to_string(&files.custom).unwrap(), original);
    }
    for base in [
        format!("http://user:{marker}@127.0.0.1:1/v1"),
        format!("http://127.0.0.1:1/v1?token={marker}"),
        format!("http://127.0.0.1:1/v1#{marker}"),
    ] {
        let config = draft("responses", &base);
        error(
            client
                .put(format!("{}/api/providers/local", server.url))
                .json(&config)
                .send()
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
            marker,
        )
        .await;
        error(
            client
                .post(format!("{}/api/providers/local/test", server.url))
                .json(&json!({"draft":config}))
                .send()
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
            marker,
        )
        .await;
        assert_eq!(std::fs::read_to_string(&files.custom).unwrap(), original);
    }
    for name in ["bad.name", "bad%20name", "bad%2Fname"] {
        error(
            client
                .put(format!("{}/api/providers/{name}", server.url))
                .json(&draft("responses", "http://127.0.0.1:1"))
                .send()
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
            marker,
        )
        .await;
    }
    error(
        client
            .put(format!("{}/api/providers/local", server.url))
            .header("content-type", "application/json")
            .body(format!("{{\"kind\":\"{marker}"))
            .send()
            .await
            .unwrap(),
        StatusCode::BAD_REQUEST,
        marker,
    )
    .await;
    error(
        client
            .post(format!("{}/api/providers/local/test", server.url))
            .json(&json!({"draft":draft("responses", "http://127.0.0.1:1"), "api_key":marker}))
            .send()
            .await
            .unwrap(),
        StatusCode::BAD_REQUEST,
        marker,
    )
    .await;
    error(
        client
            .put(format!("{}/api/roles/main", server.url))
            .json(&json!({"model":"demo/one", "token":marker}))
            .send()
            .await
            .unwrap(),
        StatusCode::BAD_REQUEST,
        marker,
    )
    .await;
    assert_eq!(std::fs::read_to_string(&files.custom).unwrap(), original);
    server.stop().await;
}

#[tokio::test]
async fn model_checks_are_read_only_and_distinguish_reachability_from_auth() {
    let files = Files::new();
    let original = "# unsaved test leaves this unchanged\n[providers.local]\nkind = 'scripted'\n";
    write(&files.custom, original);
    let mock = Server::start(
        Router::new()
            .route(
                "/listed/models",
                get(|| async { Json(json!({"data":[{"id":"model-one"}]})) }),
            )
            .route(
                "/unauthorized/models",
                get(|| async { (StatusCode::UNAUTHORIZED, "private response ignored") }),
            )
            .route("/missing/models", get(|| async { StatusCode::NOT_FOUND })),
    )
    .await;
    let server = files.setup(files.options(true)).await;
    let client = client();
    for (path, auth) in [
        ("listed", "unchecked"),
        ("unauthorized", "failed"),
        ("missing", "unchecked"),
    ] {
        let mut config = draft("responses", &format!("{}/{path}", mock.url));
        config["models"] = json!([{"id":"model-one"},{"id":"absent"}]);
        let result = payload(
            client
                .post(format!("{}/api/providers/local/test", server.url))
                .json(&json!({"draft":config}))
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(result["reachable"], true, "{path}");
        assert_eq!(result["auth"], auth, "{path}");
        if path == "listed" {
            assert_eq!(result["models_listed"], json!(["model-one"]));
            assert_eq!(
                result["configured_models"][0],
                json!({"id":"model-one", "listed":true})
            );
            assert_eq!(
                result["configured_models"][1],
                json!({"id":"absent", "listed":false})
            );
        }
        assert_eq!(std::fs::read_to_string(&files.custom).unwrap(), original);
        assert_eq!(listing(&client, &server).await["restart_needed"], false);
    }
    let scripted = payload(
        client
            .post(format!("{}/api/providers/local/test", server.url))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(scripted["auth"], "not_applicable");
    server.stop().await;
    mock.stop().await;
}

#[tokio::test]
async fn missing_credentials_still_probe_reachability_and_do_not_follow_redirects() {
    let files = Files::new();
    write(&files.custom, "# redirect fixture\n");
    let reached = Arc::new(AtomicUsize::new(0));
    let hit = reached.clone();
    let destination = Server::start(Router::new().route(
        "/models",
        get(move || {
            let hit = hit.clone();
            async move {
                hit.fetch_add(1, Ordering::SeqCst);
                Json(json!({"data":[]}))
            }
        }),
    ))
    .await;
    let location = format!("{}/models", destination.url);
    let source = Server::start(Router::new().route(
        "/models",
        get(move || {
            let location = location.clone();
            async move { (StatusCode::FOUND, [("location", location)]) }
        }),
    ))
    .await;
    let server = files.setup(files.options(true)).await;
    let client = client();
    let mut config = draft("responses", &source.url);
    config["key_env"] = json!("BP_WEB_MISSING_CREDENTIAL_75462_KEY");
    let result = payload(
        client
            .post(format!("{}/api/providers/local/test", server.url))
            .json(&json!({"draft":config}))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(result["reachable"], true);
    assert_eq!(result["auth"], "unchecked");
    assert_eq!(reached.load(Ordering::SeqCst), 0, "probe followed redirect");
    assert_eq!(result["config"]["ok"], false);
    let detail = result["config"]["error"]
        .as_str()
        .unwrap()
        .to_ascii_lowercase();
    assert!(detail.contains("missing") || detail.contains("not set"));
    server.stop().await;
    source.stop().await;
    destination.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn credentials_are_used_for_local_headers_but_never_reflected_or_persisted() {
    let files = Files::new();
    let key_name = format!("BP_WEB_TEST_{}_KEY", uuid::Uuid::new_v4().simple());
    let secret = format!("fixture-secret-{}", uuid::Uuid::new_v4().simple());
    write(
        &files.custom,
        &format!(
            "[providers.local]\nkind='responses'\nbase_url='http://127.0.0.1:1'\nkey_env='{key_name}'\n"
        ),
    );
    let headers = Arc::new(Mutex::new(Vec::new()));
    let recorded = headers.clone();
    let malicious = Server::start(Router::new().fallback(move |request: Request| {
        let recorded = recorded.clone();
        async move {
            let auth = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            let api_key = request
                .headers()
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            recorded.lock().unwrap().push((
                request.method().clone(),
                request.uri().path().to_owned(),
                auth.clone(),
                api_key.clone(),
                request.headers().contains_key("anthropic-version"),
            ));
            // Even successful models IDs and failure bodies can echo credentials.
            if request.uri().path().starts_with("/failure") {
                (StatusCode::UNAUTHORIZED, format!("{auth} {api_key}")).into_response()
            } else {
                Json(json!({"data":[{"id":auth}, {"id":api_key}, {"id":"model-one"}]}))
                    .into_response()
            }
        }
    }))
    .await;
    // The invocation supplies the credential; the host environment has no
    // value for this unique variable. No process-global mutation is needed.
    let mut options = files.options(true);
    options.env.push((key_name.clone(), secret.clone()));
    let server = files.setup(options).await;
    let client = client();
    assert_eq!(
        provider(&listing(&client, &server).await, "local")["key_present"],
        true
    );
    for kind in [
        "responses",
        "chat-completions",
        "anthropic-messages",
        "decisions",
    ] {
        for prefix in ["", "/failure"] {
            let mut config = draft(kind, &format!("{}{prefix}", malicious.url));
            config["key_env"] = json!(key_name);
            let response = client
                .post(format!("{}/api/providers/local/test", server.url))
                .json(&json!({"draft":config}))
                .send()
                .await
                .unwrap();
            assert!(response.status().is_success());
            let text = response.text().await.unwrap();
            assert!(
                !text.contains(&secret),
                "probe returned a credential from {kind}"
            );
        }
    }
    {
        let calls = headers.lock().unwrap();
        assert_eq!(calls.len(), 8);
        for (method, path, auth, api_key, version) in calls.iter() {
            assert_eq!(method, reqwest::Method::GET);
            assert!(path.ends_with("/models"));
            if !api_key.is_empty() {
                assert_eq!(api_key, &secret);
                assert!(*version);
            } else {
                assert_eq!(auth, &format!("Bearer {secret}"));
            }
        }
    }
    let text = client
        .get(format!("{}/api/providers", server.url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!text.contains(&secret));
    assert!(
        !std::fs::read_to_string(&files.custom)
            .unwrap()
            .contains(&secret)
    );
    server.stop().await;
    malicious.stop().await;
}

#[tokio::test]
async fn codex_checks_only_local_auth_shape_and_host_reachability() {
    let files = Files::new();
    let original = "# auth-path fixture\n";
    write(&files.custom, original);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let host = Server::start(Router::new().fallback(move |request: Request| {
        let captured = captured.clone();
        async move {
            captured.lock().unwrap().push((
                request.method().clone(),
                request.uri().path().to_owned(),
                request.headers().contains_key("authorization"),
            ));
            StatusCode::OK
        }
    }))
    .await;
    let auth_path = files.custom.parent().unwrap().join("fixture-auth.json");
    let secret = "LOCAL_CODEX_TOKEN_NEVER_RETURN_94372";
    write(
        &auth_path,
        &json!({"tokens":{"access_token":secret,"account_id":"fixture-account"}}).to_string(),
    );
    let server = files.setup(files.options(true)).await;
    let client = client();
    // The relative auth path must resolve beside the custom target without saving.
    let config = json!({"kind":"codex", "base_url":format!("{}/backend-api/codex", host.url), "auth_file":"fixture-auth.json", "models":[]});
    let good = payload(
        client
            .post(format!("{}/api/providers/local/test", server.url))
            .json(&json!({"draft":config}))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(good["config"]["ok"], true);
    assert_eq!(good["reachable"], true);
    assert_eq!(good["auth"], "unchecked");
    assert!(!good.to_string().contains(secret));
    for contents in [
        format!("{{malformed-{secret}"),
        json!({"tokens":{"access_token":"","account_id":"fixture-account"}}).to_string(),
    ] {
        write(&auth_path, &contents);
        let failed = payload(
            client
                .post(format!("{}/api/providers/local/test", server.url))
                .json(&json!({"draft":config}))
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(failed["config"]["ok"], false);
        assert_eq!(failed["reachable"], true);
        assert_eq!(failed["auth"], "unchecked");
        assert!(!failed.to_string().contains(secret));
    }
    std::fs::remove_file(&auth_path).unwrap();
    let missing = payload(
        client
            .post(format!("{}/api/providers/local/test", server.url))
            .json(&json!({"draft":config}))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(missing["config"]["ok"], false);
    assert_eq!(missing["reachable"], true);
    assert_eq!(std::fs::read_to_string(&files.custom).unwrap(), original);
    assert_eq!(listing(&client, &server).await["restart_needed"], false);
    {
        let calls = requests.lock().unwrap();
        assert_eq!(calls.len(), 4);
        for (method, path, auth) in calls.iter() {
            assert_eq!(method, reqwest::Method::GET);
            assert_eq!(path, "/");
            assert!(
                !auth,
                "Codex token was sent during a host-reachability check"
            );
        }
    }
    assert!(
        !std::fs::read_to_string(&files.custom)
            .unwrap()
            .contains(secret)
    );
    server.stop().await;
    host.stop().await;
}

#[tokio::test]
async fn configured_models_are_checked_beyond_the_returned_list_limit() {
    let files = Files::new();
    let original = "# capped model-list fixture\n";
    write(&files.custom, original);
    // Model IDs have no configured length limit; only the response body and
    // returned list length are bounded.
    let configured_id = format!("model-500{}", "x".repeat(1024));
    let mock_id = configured_id.clone();
    let mock = Server::start(Router::new().route(
        "/models",
        get(move || {
            let mock_id = mock_id.clone();
            async move {
                let mut models: Vec<_> = (0..501)
                    .map(|index| json!({"id":format!("model-{index}")}))
                    .collect();
                models[500]["id"] = json!(mock_id);
                Json(json!({"data":models}))
            }
        }),
    ))
    .await;
    let server = files.setup(files.options(true)).await;
    let client = client();
    let mut config = draft("responses", &mock.url);
    config["models"] = json!([{"id":configured_id}, {"id":"absent"}]);
    let result = payload(
        client
            .post(format!("{}/api/providers/local/test", server.url))
            .json(&json!({"draft":config}))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(result["reachable"], true);
    let returned = result["models_listed"].as_array().unwrap();
    assert_eq!(returned.len(), 500);
    assert!(!returned.contains(&json!(configured_id)));
    assert_eq!(
        result["configured_models"][0],
        json!({"id":configured_id, "listed":true})
    );
    assert_eq!(
        result["configured_models"][1],
        json!({"id":"absent", "listed":false})
    );
    assert_eq!(std::fs::read_to_string(&files.custom).unwrap(), original);
    assert_eq!(listing(&client, &server).await["restart_needed"], false);
    server.stop().await;
    mock.stop().await;
}

#[tokio::test]
async fn model_checks_bound_large_bodies_and_request_time() {
    let files = Files::new();
    write(&files.custom, "# bounded fixture\n");
    let mock = Server::start(
        Router::new()
            .route(
                "/large/models",
                get(|| async { "x".repeat(2 * 1024 * 1024 + 1) }),
            )
            .route(
                "/slow/models",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Json(json!({"data":[]}))
                }),
            ),
    )
    .await;
    let server = files.setup(files.options(true)).await;
    let client = client();
    let large = client
        .post(format!("{}/api/providers/local/test", server.url))
        .json(&json!({"draft":draft("responses", &format!("{}/large", mock.url))}))
        .send()
        .await
        .unwrap();
    assert!(large.status().is_success());
    let text = large.text().await.unwrap();
    assert!(text.len() < 16_384, "unbounded upstream body was returned");
    let capped: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(capped["reachable"], true);
    assert!(capped["models_listed"].is_null());
    assert!(
        capped["detail"]
            .as_str()
            .unwrap()
            .contains("response limit")
    );
    let start = Instant::now();
    let slow = payload(
        client
            .post(format!("{}/api/providers/local/test", server.url))
            .json(&json!({"draft":draft("responses", &format!("{}/slow", mock.url))}))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(!slow["reachable"].as_bool().unwrap());
    assert!(
        start.elapsed() < Duration::from_secs(13),
        "probe exceeded its ten-second deadline"
    );
    server.stop().await;
    mock.stop().await;
}

#[tokio::test]
async fn unreachable_host_reports_a_connect_failure_without_sending_a_credential() {
    let files = Files::new();
    let original = "# closed port fixture\n";
    write(&files.custom, original);
    let key_name = format!("BP_WEB_PROBE_{}_KEY", uuid::Uuid::new_v4().simple());
    let secret = "PRIVATE_PROBE_KEY_FIXTURE_30318";
    let mut options = files.options(true);
    // Supply a synthetic credential to the loader without process-global mutation.
    options.env.push((key_name.clone(), secret.into()));
    let server = files.setup(options).await;
    let client = client();
    // A listener bound and dropped here leaves its loopback port closed.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut config = draft("chat-completions", &format!("http://127.0.0.1:{port}/v1"));
    config["key_env"] = json!(key_name);
    let result = payload(
        client
            .post(format!("{}/api/providers/local/test", server.url))
            .json(&json!({"draft":config}))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(result["reachable"], false);
    assert_eq!(result["detail"], "Could not connect to the host");
    assert!(!result.to_string().contains(secret));
    assert_eq!(std::fs::read_to_string(&files.custom).unwrap(), original);
    server.stop().await;
}
