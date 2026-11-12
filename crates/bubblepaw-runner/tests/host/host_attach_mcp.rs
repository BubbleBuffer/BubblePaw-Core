//! Attach MCP environment through real processes: one owner host and two attached
//! `run` clients that use the same MCP server configuration with different client
//! values. A local axum server records the headers it receives. A wrapper records
//! only the fixture variables each stdio child received, never the whole
//! environment, into a directory outside the data directory. Secret values are
//! scanned in data, trace, stdout and stderr files.
//!
//! The provider is the fake extension as `fake/direct-model` on the default
//! explicitly selected JS execution mode. MCP tools are reached through `js` prompts, because the fake
//! extension can only request its own `echo` or `js` tools; `native.toml` would
//! remove `js`. The owner's exit marker is set in the owner's own config
//! (`extensions.fake.env`), not in the owner process environment, so attached
//! clients never inherit it. Linux only. Every wait is bounded; every process is
//! one this test spawned, and `kill_on_drop` plus explicit shutdown leave none behind.
#![cfg(unix)]

use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    net::TcpListener,
    process::{Child, Command},
};

const BIN: &str = env!("CARGO_BIN_EXE_bubblepaw");
const FAKE_EXT: &str = env!("CARGO_BIN_EXE_bubblepaw-fake-extension");
const FAKE_MCP: &str = env!("CARGO_BIN_EXE_bubblepaw-runner-fake-mcp");
/// Same capabilities as the `host_attach.rs` extension fixture.
const CAPABILITIES: &str = r#"--capabilities={"output_schema":true,"reasoning_effort":true}"#;
/// Upper bound for any single wait. Generous for a loaded runner; failures stay bounded.
const WAIT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(20);

/// Host-side variable names referenced by the shared configuration. These are the
/// only host variables this test reads; nothing else is snapshotted.
const HTTP_VAR: &str = "FIXTURE_HTTP_TOKEN";
const STDIO_VAR: &str = "FIXTURE_STDIO_TOKEN";
const MISSING_VAR: &str = "FIXTURE_MISSING_TOKEN";
/// Set on the owner only; a stdio child should inherit it because clients do not forward it.
const HOST_ONLY_VAR: &str = "FIXTURE_HOST_ONLY";
/// Set on one client only; it must never reach any stdio child.
const UNRELATED_VAR: &str = "FIXTURE_UNRELATED";

/// Synthetic secrets. Each is unique so a substring scan has no false positives.
const HOST_HTTP: &str = "host-http-secret-0001";
const HOST_STDIO: &str = "host-stdio-secret-0001";
const HOST_MISSING: &str = "host-missing-secret-0001";
const HOST_ONLY: &str = "host-only-value-0001";
const A_HTTP: &str = "client-a-http-secret-0001";
const B_HTTP: &str = "client-b-http-secret-0001";
const A_STDIO: &str = "client-a-stdio-secret-0001";
const B_STDIO: &str = "client-b-stdio-secret-0001";
const UNRELATED_A: &str = "unrelated-a-secret-0001";

/// The warning text for a required HTTP credential variable that is absent (`http.rs`).
const MISSING_MESSAGE: &str = "required MCP HTTP credential environment variable is missing";

type Env = BTreeMap<String, String>;

/// Records every HTTP request's method, `text` argument and `X-Fixture-Token` header.
struct Recorder {
    log: PathBuf,
    lock: Mutex<()>,
}

async fn serve_mcp(
    State(recorder): State<Arc<Recorder>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Ok(request) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let method = request["method"].as_str().unwrap_or_default().to_owned();
    let text = request["params"]["arguments"]["text"]
        .as_str()
        .map(str::to_owned);
    let token = headers
        .get("x-fixture-token")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    {
        let _guard = recorder.lock.lock().unwrap();
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&recorder.log)
            .unwrap();
        writeln!(
            log,
            "{}",
            json!({"method": method, "text": text, "token": token})
        )
        .unwrap();
    }
    let id = request["id"].clone();
    if id.is_null() {
        return StatusCode::ACCEPTED.into_response();
    }
    let result = match method.as_str() {
        "tools/list" => {
            json!({"tools":[{"name":"echo","description":"fixture","inputSchema":{"type":"object"}}]})
        }
        "tools/call" => json!({
            "resultType": "complete",
            "content": [{"type": "text", "text": format!("echo:{}", text.unwrap_or_default())}]
        }),
        _ => return StatusCode::METHOD_NOT_ALLOWED.into_response(),
    };
    Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}

/// Temporary directory, the local MCP HTTP server and the configurations.
struct Lab {
    root: tempfile::TempDir,
    url: String,
    server: tokio::task::JoinHandle<()>,
    wrapper: PathBuf,
    /// The clients' configuration: the same servers, and no exit marker.
    config: PathBuf,
    /// The owner's configuration only: its extension alone gets the exit marker.
    owner_config: PathBuf,
    standalone_config: PathBuf,
}

impl Lab {
    async fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let recorder = Arc::new(Recorder {
            log: root.path().join("http.jsonl"),
            lock: Mutex::new(()),
        });
        let app = Router::new()
            .route("/mcp", post(serve_mcp))
            .with_state(recorder);
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        for dir in ["stdio-env", "traces"] {
            fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        // The wrapper writes only the fixture names it lists, then `exec`s the fake
        // MCP server, so the recorded pid is the MCP child's pid.
        let wrapper = root.path().join("stdio-wrapper.sh");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\n\
                 {{ printf 'STDIO_TOKEN=%s\\n' \"${{STDIO_TOKEN-}}\"; \
                 printf 'FIXTURE_HOST_ONLY=%s\\n' \"${{FIXTURE_HOST_ONLY-}}\"; \
                 printf 'FIXTURE_UNRELATED=%s\\n' \"${{FIXTURE_UNRELATED-}}\"; }} > \"{}/$$.env\"\n\
                 exec \"{FAKE_MCP}\" \"$@\"\n",
                root.path().join("stdio-env").display()
            ),
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        let mut lab = Self {
            root,
            url,
            server,
            wrapper,
            config: PathBuf::new(),
            owner_config: PathBuf::new(),
            standalone_config: PathBuf::new(),
        };
        let marker = lab.exit_marker();
        lab.config = lab.write_config("config.toml", &lab.data(), None);
        lab.owner_config = lab.write_config("owner.toml", &lab.data(), Some(marker.as_path()));
        lab.standalone_config =
            lab.write_config("standalone.toml", &lab.path("standalone-data"), None);
        lab
    }

    fn root(&self) -> &Path {
        self.root.path()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root().join(name)
    }

    fn data(&self) -> PathBuf {
        self.path("data")
    }

    fn traces(&self) -> PathBuf {
        self.path("traces")
    }

    fn stdio_dir(&self) -> PathBuf {
        self.path("stdio-env")
    }

    fn http_log(&self) -> PathBuf {
        self.path("http.jsonl")
    }

    fn exit_marker(&self) -> PathBuf {
        self.path("extension-exited")
    }

    /// Same servers for every process: an HTTP server with a header from a client
    /// variable, a stdio server with `env_from`, and a server whose variable may be absent.
    /// `exit_marker` is set on the fake extension only, and only in the owner's config.
    fn write_config(&self, name: &str, data: &Path, exit_marker: Option<&Path>) -> PathBuf {
        let mut fake = json!({
            "command": FAKE_EXT,
            "args": [CAPABILITIES],
            "cwd": self.root()
        });
        if let Some(marker) = exit_marker {
            fake["env"] = json!({"BP_FAKE_EXIT_MARKER": marker});
        }
        let value = json!({
            "model": {"main": "fake/direct-model"},
            "agent": {"execution_mode": "js"},
            "context": {"auto_summarize": false},
            "storage": {"data_dir": data},
            "extensions": {"fake": fake},
            "mcp": {"servers": {
                "remote": {
                    "url": self.url,
                    "headers_from": {"X-Fixture-Token": HTTP_VAR},
                    "startup_timeout_secs": 5, "call_timeout_secs": 5
                },
                "stdio": {
                    "command": self.wrapper,
                    "env_from": {"STDIO_TOKEN": STDIO_VAR},
                    "startup_timeout_secs": 5, "call_timeout_secs": 5
                },
                "missing": {
                    "url": self.url,
                    "headers_from": {"X-Missing-Token": MISSING_VAR},
                    "startup_timeout_secs": 5, "call_timeout_secs": 5
                }
            }}
        });
        let path = self.path(name);
        fs::write(&path, toml::to_string(&value).unwrap()).unwrap();
        path
    }

    /// A `bubblepaw` command. The test's own environment is inherited, minus any
    /// fixture variables; `env` adds only the named values for this process.
    fn command(&self, config: &Path, trace: &str, env: &[(&str, &str)], sub: &[&str]) -> Command {
        let mut command = Command::new(BIN);
        for (key, _) in std::env::vars_os() {
            if let Some(name) = key.to_str()
                && (name.starts_with("BUBBLEPAW_") || name.starts_with("FIXTURE_"))
            {
                command.env_remove(name);
            }
        }
        command
            .arg("--config")
            .arg(config)
            .arg("--workspace")
            .arg(self.root())
            .args(["--approve-all", "--json", "--trace-mode", "contents"])
            .arg("--trace-file")
            .arg(self.traces().join(format!("{trace}.jsonl")))
            .args(sub)
            .env("CODEX_HOME", self.path("absent-codex"))
            .stdin(Stdio::null())
            .kill_on_drop(true);
        for (name, value) in env {
            command.env(name, value);
        }
        command
    }

    /// Starts the owner with `run sleep`, which keeps it hosting until it is killed.
    async fn start_owner(&self, data: &Path, env: &[(&str, &str)]) -> Proc {
        let mut owner = Proc::spawn(
            self.root(),
            "owner",
            self.command(&self.owner_config, "owner", env, &["run", "sleep 8000"]),
        );
        owner.until("host.json", || host_json(data).is_some()).await;
        owner
    }

    /// Kills the owner this test started and waits until its extension has written
    /// the exit marker, which only the owner's extension config asks for.
    async fn stop_owner(&self, owner: &mut Proc) {
        owner.signal(libc::SIGKILL);
        owner.finish().await;
        let deadline = Instant::now() + WAIT;
        while !self.exit_marker().exists() {
            assert!(Instant::now() < deadline, "owner extension did not exit");
            tokio::time::sleep(POLL).await;
        }
    }

    /// Every `tools/call` request the fixture server received, in arrival order.
    fn http_calls(&self) -> Vec<Value> {
        fs::read_to_string(self.http_log())
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|record| record["method"] == "tools/call")
            .collect()
    }

    /// The fixture variables each stdio child recorded, keyed by that child's pid.
    /// Each file holds exactly the names the wrapper prints, never the full environment.
    fn stdio_children(&self) -> Vec<(u32, Env)> {
        fs::read_dir(self.stdio_dir())
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let pid = path.file_stem().unwrap().to_str().unwrap().parse().unwrap();
                let env = fs::read_to_string(&path)
                    .unwrap()
                    .lines()
                    .filter_map(|line| line.split_once('='))
                    .map(|(name, value)| (name.to_owned(), value.to_owned()))
                    .collect();
                (pid, env)
            })
            .collect()
    }

    /// Files under the lab that hold a secret. The HTTP log and the stdio snapshots are
    /// this fixture's own records outside data, traces and output, so they are skipped.
    fn secret_hits(&self, secrets: &[&str]) -> Vec<String> {
        let skip = [self.stdio_dir(), self.http_log()];
        let mut hits = Vec::new();
        let mut pending = vec![self.root().to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if skip.contains(&path) {
                    continue;
                }
                if path.is_dir() {
                    pending.push(path);
                    continue;
                }
                // A deliberately killed owner leaves its Unix socket behind.
                // Only regular files can contain persisted credentials.
                if !path.is_file() {
                    continue;
                }
                let text = String::from_utf8_lossy(&fs::read(&path).unwrap()).into_owned();
                for secret in secrets {
                    if text.contains(secret) {
                        hits.push(format!("{} holds {secret}", path.display()));
                    }
                }
            }
        }
        hits
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// A `bubblepaw` process whose output goes to files, so a full pipe cannot block it.
struct Proc {
    name: &'static str,
    child: Child,
    pid: u32,
    out: PathBuf,
    err: PathBuf,
}

impl Proc {
    fn spawn(dir: &Path, name: &'static str, mut command: Command) -> Self {
        let out = dir.join(format!("{name}.out"));
        let err = dir.join(format!("{name}.err"));
        command
            .stdout(File::create(&out).unwrap())
            .stderr(File::create(&err).unwrap());
        let child = command
            .spawn()
            .unwrap_or_else(|error| panic!("spawn {name}: {error}"));
        let pid = child.id().expect("child pid");
        Self {
            name,
            child,
            pid,
            out,
            err,
        }
    }

    fn stdout(&self) -> String {
        fs::read_to_string(&self.out).unwrap_or_default()
    }

    fn stderr(&self) -> String {
        fs::read_to_string(&self.err).unwrap_or_default()
    }

    /// Stdout and stderr together, for checks that must find a warning on either stream.
    fn output(&self) -> String {
        format!("{}{}", self.stdout(), self.stderr())
    }

    /// Sends one signal to this process's own pid. Only while it is still running.
    fn signal(&mut self, signal: libc::c_int) {
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "{} already exited: {}",
            self.name,
            self.stderr()
        );
        assert_eq!(unsafe { libc::kill(self.pid as libc::pid_t, signal) }, 0);
    }

    async fn finish(&mut self) -> ExitStatus {
        match tokio::time::timeout(WAIT, self.child.wait()).await {
            Ok(status) => status.unwrap(),
            Err(_) => panic!(
                "{} did not exit; stdout: {}; stderr: {}",
                self.name,
                self.stdout(),
                self.stderr()
            ),
        }
    }

    /// Waits until `ready` holds. Fails at once if this process exits first.
    async fn until(&mut self, what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + WAIT;
        loop {
            if ready() {
                return;
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!(
                    "{} exited ({status}) before {what}; stdout: {}; stderr: {}",
                    self.name,
                    self.stdout(),
                    self.stderr()
                );
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(POLL).await;
        }
    }
}

fn host_json(data: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(data.join("host.json")).ok()?).ok()
}

/// A `js` prompt that calls one MCP echo tool and prints its result or its error.
fn echo_prompt(tool: &str, text: &str) -> String {
    format!(
        "js try {{ const value = await tools.{tool}({{text:'{text}'}}); text(JSON.stringify(value)); }} catch (e) {{ text(e.message); }}"
    )
}

/// Waits until `pid` is gone, failing after the bounded wait.
async fn wait_gone(pid: u32) {
    let deadline = Instant::now() + WAIT;
    // SAFETY: signal 0 only checks whether the process still exists.
    while unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        assert!(
            Instant::now() < deadline,
            "stdio child {pid} is still running"
        );
        tokio::time::sleep(POLL).await;
    }
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

/// The snapshot of the first stdio child whose `name` holds `value`.
fn child_with(children: &[(u32, Env)], name: &str, value: &str) -> Env {
    children
        .iter()
        .find(|(_, env)| env.get(name).map(String::as_str) == Some(value))
        .unwrap_or_else(|| panic!("no stdio child with {name}={value}: {children:?}"))
        .1
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn two_attached_sessions_keep_their_own_client_values() {
    let lab = Lab::start().await;
    let data = lab.data();
    let host_env = [
        (HTTP_VAR, HOST_HTTP),
        (STDIO_VAR, HOST_STDIO),
        (HOST_ONLY_VAR, HOST_ONLY),
    ];
    let mut owner = lab.start_owner(&data, &host_env).await;

    let a_env = [
        (HTTP_VAR, A_HTTP),
        (STDIO_VAR, A_STDIO),
        (UNRELATED_VAR, UNRELATED_A),
    ];
    let b_env = [(HTTP_VAR, B_HTTP), (STDIO_VAR, B_STDIO)];
    let a_prompt = echo_prompt("mcp__remote__echo", "session-a");
    let b_prompt = echo_prompt("mcp__remote__echo", "session-b");
    // Both clients attach at once to the same owner and the same server configuration.
    let mut a = Proc::spawn(
        lab.root(),
        "client-a",
        lab.command(&lab.config, "client-a", &a_env, &["run", a_prompt.as_str()]),
    );
    let mut b = Proc::spawn(
        lab.root(),
        "client-b",
        lab.command(&lab.config, "client-b", &b_env, &["run", b_prompt.as_str()]),
    );
    let (status_a, status_b) = (a.finish().await, b.finish().await);
    assert!(status_a.success(), "{}", a.output());
    assert!(status_b.success(), "{}", b.output());
    assert!(a.stdout().contains("echo:session-a"), "{}", a.output());
    assert!(b.stdout().contains("echo:session-b"), "{}", b.output());

    // Each session's HTTP calls carry only that session's header value.
    let calls = lab.http_calls();
    for (session, token) in [("session-a", A_HTTP), ("session-b", B_HTTP)] {
        let seen: Vec<&Value> = calls
            .iter()
            .filter(|call| call["text"] == session)
            .collect();
        assert!(!seen.is_empty(), "no HTTP call for {session}: {calls:?}");
        assert!(
            seen.iter().all(|call| call["token"] == token),
            "{session} sent the wrong header: {seen:?}"
        );
    }

    // Same server name, different values: two independent stdio children, each with
    // its own client's value and never the other's.
    let children = lab.stdio_children();
    let a_child = child_with(&children, "STDIO_TOKEN", A_STDIO);
    let b_child = child_with(&children, "STDIO_TOKEN", B_STDIO);
    assert_ne!(a_child, b_child);
    assert!(!a_child.values().any(|value| value.contains(B_STDIO)));
    assert!(!b_child.values().any(|value| value.contains(A_STDIO)));
    // Host inheritance applies to a variable the client did not forward.
    assert_eq!(
        a_child.get(HOST_ONLY_VAR).map(String::as_str),
        Some(HOST_ONLY)
    );
    // A variable only client A had is never transmitted: every child records it empty.
    assert!(
        children
            .iter()
            .all(|(_, env)| env.get(UNRELATED_VAR).map(String::as_str) == Some("")),
        "{children:?}"
    );
    assert!(
        children
            .iter()
            .all(|(_, env)| env.values().all(|value| !value.contains(UNRELATED_A))),
        "{children:?}"
    );

    lab.stop_owner(&mut owner).await;
    let stdio_pids: Vec<u32> = children.iter().map(|(pid, _)| *pid).collect();
    for pid in stdio_pids {
        wait_gone(pid).await;
    }

    // Neither client secret appears in data, traces, stdout or stderr.
    let hits = lab.secret_hits(&[A_HTTP, B_HTTP, A_STDIO, B_STDIO]);
    assert!(hits.is_empty(), "secrets leaked: {hits:?}");
    let traces: Vec<PathBuf> = fs::read_dir(lab.traces())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert!(!traces.is_empty(), "contents traces were not written");
}

#[path = "host_attach_mcp/missing_credentials.rs"]
mod missing_credentials;
