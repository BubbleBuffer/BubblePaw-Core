//! Shared session control: one slash command, one behaviour, on stdio and web.
use anyhow::Result;
use bubblepaw_runner::session_control::SlashCommand;
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[cfg(feature = "web")]
use bubblepaw_core::{ContextStrategy, Harness, HarnessConfig, Store, ToolRegistry};
#[cfg(feature = "web")]
use bubblepaw_runner::{
    provider::ScriptedProvider,
    web::{WebConfig, WebServices, serve_with_services},
};
#[cfg(feature = "web")]
use std::sync::Arc;

/// The real `bubblepaw stdio` binary, driven over JSON-RPC lines.
struct StdioHost {
    child: tokio::process::Child,
    input: tokio::process::ChildStdin,
    output: BufReader<tokio::process::ChildStdout>,
    next: u64,
}

impl StdioHost {
    fn start(workspace: &Path, data: &Path) -> Self {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_bubblepaw"))
            .args(["--provider", "demo", "--no-auto-summarize", "--workspace"])
            .arg(workspace)
            .args([
                "--set",
                &format!("storage.data_dir={}", data.display()),
                "stdio",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        Self {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
            next: 1,
        }
    }

    /// The whole response, with either `result` or `error`.
    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        let line = format!(
            "{}\n",
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        );
        self.input.write_all(line.as_bytes()).await.unwrap();
        self.input.flush().await.unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let mut line = String::new();
                assert!(
                    self.output.read_line(&mut line).await.unwrap() > 0,
                    "host closed stdout"
                );
                let value: Value = serde_json::from_str(&line).unwrap();
                if value["id"] == id {
                    return value;
                }
            }
        })
        .await
        .unwrap()
    }

    async fn ok(&mut self, method: &str, params: Value) -> Value {
        let response = self.call(method, params).await;
        assert!(response.get("error").is_none(), "{method}: {response}");
        response["result"].clone()
    }

    async fn stop(mut self) {
        self.ok("shutdown", json!({})).await;
        drop(self.input);
        assert!(
            tokio::time::timeout(Duration::from_secs(10), self.child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
}

/// The real HTTP router on a local port, with the scripted provider.
#[cfg(feature = "web")]
struct WebHost {
    base: String,
    client: reqwest::Client,
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<()>>,
    _assets: tempfile::TempDir,
}

#[cfg(feature = "web")]
impl WebHost {
    async fn start(store: &Store, workspace: &Path, services: WebServices) -> Result<Self> {
        let harness = Arc::new(Harness::new(
            store.clone(),
            Arc::new(ScriptedProvider::new("done")),
            ToolRegistry::new(),
            HarnessConfig::default(),
        ));
        let assets = tempfile::tempdir()?;
        std::fs::write(assets.path().join("index.html"), "fixture")?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let (stop, receive) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(serve_with_services(
            listener,
            harness,
            store.clone(),
            WebConfig {
                workspace: workspace.display().to_string(),
                model: "scripted".into(),
                provider: "demo".into(),
                read_only: false,
            },
            assets.path().to_path_buf(),
            services,
            async {
                let _ = receive.await;
            },
        ));
        Ok(Self {
            base,
            client: reqwest::Client::new(),
            stop,
            task,
            _assets: assets,
        })
    }

    async fn post(&self, path: &str, body: Value) -> (reqwest::StatusCode, Value) {
        let response = self
            .client
            .post(format!("{}{path}", self.base))
            .header("Origin", &self.base)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let value = response.json::<Value>().await.unwrap_or(Value::Null);
        (status, value)
    }

    async fn shutdown(self) -> Result<()> {
        let _ = self.stop.send(());
        self.task.await?
    }
}

#[cfg(feature = "web")]
fn keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .expect("session JSON is an object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

#[test]
fn slash_commands_parse_only_whole_commands() {
    assert_eq!(
        SlashCommand::parse("/rename Ship it"),
        Some(SlashCommand::Rename("Ship it"))
    );
    assert_eq!(
        SlashCommand::parse("/rewind msg-7"),
        Some(SlashCommand::Rewind("msg-7"))
    );
    assert_eq!(
        SlashCommand::parse("/compact"),
        Some(SlashCommand::Compact(None))
    );
    assert_eq!(
        SlashCommand::parse("/compact keep the plan"),
        Some(SlashCommand::Compact(Some("keep the plan")))
    );
    assert_eq!(
        SlashCommand::parse("/reload"),
        Some(SlashCommand::Reload(None))
    );
    assert_eq!(
        SlashCommand::parse("/reload work"),
        Some(SlashCommand::Reload(Some("work")))
    );
    assert_eq!(SlashCommand::parse("/renamed title"), None);
    assert_eq!(SlashCommand::parse("/reload a b"), None);
    assert_eq!(SlashCommand::parse("please /compact"), None);
    assert!(!SlashCommand::Reload(None).needs_session());
    assert!(SlashCommand::Compact(None).needs_session());
}

#[cfg(feature = "web")]
#[tokio::test]
async fn rename_command_gives_the_same_session_over_stdio_and_web() -> Result<()> {
    let root = tempfile::tempdir()?;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace)?;

    let mut host = StdioHost::start(&workspace, &root.path().join("data"));
    let stdio_session = host.ok("session.open", json!({"title":"before"})).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let stdio = host
        .ok(
            "run.start",
            json!({"session_id":stdio_session,"text":"/rename Renamed"}),
        )
        .await;
    host.stop().await;

    let store = Store::in_memory()?;
    let web_session = store.create_session("before")?.id;
    let web_workspace = tempfile::tempdir()?;
    let web = WebHost::start(&store, web_workspace.path(), WebServices::default()).await?;
    let (status, web_body) = web
        .post(
            &format!("/api/sessions/{web_session}/run"),
            json!({"text":"/rename Renamed"}),
        )
        .await;
    web.shutdown().await?;

    assert_eq!(status, reqwest::StatusCode::OK, "{web_body}");
    assert_eq!(stdio["id"], stdio_session);
    assert_eq!(web_body["id"], web_session);
    assert_eq!(stdio["title"], "Renamed");
    assert_eq!(web_body["title"], "Renamed");
    assert_eq!(store.session(&web_session)?.title, "Renamed");
    assert_eq!(keys(&stdio), keys(&web_body));
    Ok(())
}

#[tokio::test]
async fn session_bound_to_another_workspace_is_refused_over_stdio() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (served, bound) = (root.path().join("served"), root.path().join("bound"));
    let data = root.path().join("data");
    std::fs::create_dir(&served)?;
    std::fs::create_dir(&bound)?;

    // Bind the session to `bound` through the shared data dir, then serve `served`.
    let mut owner = StdioHost::start(&bound, &data);
    let session = owner.ok("session.open", json!({"title":"bound"})).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    owner.stop().await;

    let mut host = StdioHost::start(&served, &data);
    for (method, params) in [
        (
            "run.start",
            json!({"session_id":session,"text":"/rename Stolen"}),
        ),
        ("run.cancel", json!({"session_id":session})),
    ] {
        let response = host.call(method, params).await;
        assert!(response.get("error").is_some(), "{method}: {response}");
        assert!(
            response.to_string().contains("another workspace"),
            "{method}: {response}"
        );
    }
    host.stop().await;
    Ok(())
}

#[cfg(feature = "web")]
#[tokio::test]
async fn session_bound_to_another_workspace_is_refused_over_web() -> Result<()> {
    let bound = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let session = store
        .create_session_with_strategy_and_workspace(
            "bound",
            ContextStrategy::default(),
            bound.path(),
        )?
        .id;
    let served = tempfile::tempdir()?;
    let web = WebHost::start(&store, served.path(), WebServices::default()).await?;
    for (path, body) in [
        (
            format!("/api/sessions/{session}/run"),
            json!({"text":"/rename Stolen"}),
        ),
        (format!("/api/sessions/{session}/cancel"), json!({})),
    ] {
        let (status, value) = web.post(&path, body).await;
        assert_eq!(status, reqwest::StatusCode::CONFLICT, "{path}: {value}");
        assert!(
            value["error"]
                .as_str()
                .unwrap_or_default()
                .contains("another workspace"),
            "{path}: {value}"
        );
    }
    web.shutdown().await?;
    Ok(())
}

#[cfg(feature = "web")]
#[tokio::test]
async fn web_reload_without_live_config_is_an_error_and_starts_no_run() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("reload")?.id;
    let workspace = tempfile::tempdir()?;
    let web = WebHost::start(&store, workspace.path(), WebServices::default()).await?;
    for path in [
        format!("/api/sessions/{session}/run"),
        format!("/api/sessions/{session}/steer"),
        format!("/api/sessions/{session}/followup"),
    ] {
        let (status, value) = web.post(&path, json!({"text":"/reload"})).await;
        assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{path}: {value}");
        assert!(
            value["error"]
                .as_str()
                .unwrap_or_default()
                .contains("config reload is not available in this host"),
            "{path}: {value}"
        );
    }
    web.shutdown().await?;
    assert!(
        store.runs(&session)?.is_empty(),
        "/reload must not start a run"
    );
    assert!(
        store.messages(&session)?.is_empty(),
        "/reload must not reach the model as a prompt"
    );
    Ok(())
}
