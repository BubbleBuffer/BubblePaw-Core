//! Attach integration: real `bubblepaw` processes share one data directory. The
//! first process owns the store and hosts; later `run` and `delegate` processes
//! attach through the host's local socket. The provider is the fake extension,
//! driven by its prompt commands and the `BP_FAKE_*` environment markers.
//!
//! Linux only. Every wait is bounded; every process is one this test spawned, and
//! `kill_on_drop` plus explicit shutdown leave none behind.
#![cfg(unix)]

use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs::File,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::{Duration, Instant},
};
use tokio::process::{Child, Command};

#[path = "support/proc_sockets.rs"]
mod proc_sockets;

#[path = "host_attach/delegations.rs"]
mod delegations;
#[path = "host_attach/normalize.rs"]
mod normalize;
#[path = "host_attach/sessions.rs"]
mod sessions;
use normalize::normalize;

const BIN: &str = env!("CARGO_BIN_EXE_bubblepaw");
const FAKE: &str = env!("CARGO_BIN_EXE_bubblepaw-fake-extension");
/// Upper bound for any single wait. Generous for a loaded runner; failures stay bounded.
const WAIT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(20);

/// Passes when the workspace holds `done.txt`, as the worker is told to create it.
const CHECK: &str = "test -f done.txt || { echo 'done.txt missing'; exit 1; }";
/// Passes only on its second run, so the first attempt must be reworked.
const CHECK_TWICE: &str = r#"echo x >> checks.log; test "$(wc -l < checks.log)" -ge 2"#;

/// Temporary directory with the data directory, workspaces and fixture logs.
struct Lab {
    root: tempfile::TempDir,
}

impl Lab {
    fn new() -> Self {
        let lab = Self {
            root: tempfile::tempdir().unwrap(),
        };
        let cwd = lab.root().to_str().unwrap();
        let extensions = json!([{"id":"fake","command":FAKE,"args":["--capabilities={\"output_schema\":true,\"reasoning_effort\":true}"],"cwd":cwd,"env":{"BP_FAKE_STEP_LOG":lab.steps_log()}}]);
        std::fs::write(lab.path("extensions.json"), extensions.to_string()).unwrap();
        std::fs::write(lab.path("js.toml"), "[agent]\nexecution_mode=\"js\"\n").unwrap();
        std::fs::write(
            lab.path("native.toml"),
            "[agent]\nexecution_mode=\"native\"\n",
        )
        .unwrap();
        lab
    }

    fn root(&self) -> &Path {
        self.root.path()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root().join(name)
    }

    fn dir(&self, name: &str) -> PathBuf {
        let path = self.path(name);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn data(&self) -> PathBuf {
        self.path("data")
    }

    fn steps_log(&self) -> PathBuf {
        self.path("steps.jsonl")
    }

    fn exit_marker(&self) -> PathBuf {
        self.path("extension-exited")
    }

    /// A `bubblepaw` command for `data`. `globals` precede the subcommand in `sub`.
    fn command(
        &self,
        data: &Path,
        workspace: &Path,
        model: &str,
        globals: &[&str],
        sub: &[&str],
    ) -> Command {
        let mut command = Command::new(BIN);
        command
            .args([
                "--provider",
                "demo",
                "--model",
                model,
                "--approve-all",
                "--json",
            ])
            .arg("--workspace")
            .arg(workspace)
            .arg("--data-dir")
            .arg(data)
            .arg("--extensions")
            .arg(self.path("extensions.json"))
            .args(globals)
            .args(sub)
            .env("BP_FAKE_STEP_LOG", self.steps_log())
            .stdin(Stdio::null())
            .kill_on_drop(true);
        if model == "fake/direct-model" {
            command.arg("--config").arg(self.path("native.toml"));
        } else {
            command.arg("--config").arg(self.path("js.toml"));
        }
        command
    }

    /// Every record the fake extension has logged, in arrival order: steps (with
    /// their JSON-RPC `id` and `pid`) and cancels (`{"cancel": id, "pid": pid}`).
    fn records(&self) -> Vec<Value> {
        std::fs::read_to_string(self.steps_log())
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// Step records whose last message is exactly `prompt`.
    fn prompt_records(&self, prompt: &str) -> Vec<Value> {
        self.records()
            .into_iter()
            .filter(|record| last_content(record) == Some(prompt))
            .collect()
    }

    fn steps_for(&self, prompt: &str) -> usize {
        self.prompt_records(prompt).len()
    }

    fn step_id(&self, prompt: &str) -> (u64, u64) {
        let record = &self.prompt_records(prompt)[0];
        (
            record["pid"].as_u64().unwrap(),
            record["id"].as_u64().unwrap(),
        )
    }

    fn cancel_targets(&self) -> Vec<(u64, u64)> {
        self.records()
            .iter()
            .filter_map(|record| Some((record["pid"].as_u64()?, record["cancel"].as_u64()?)))
            .collect()
    }

    async fn wait_for_extension_exit(&self) {
        let marker = self.exit_marker();
        let deadline = Instant::now() + WAIT;
        while !marker.exists() {
            assert!(Instant::now() < deadline, "extension did not exit");
            tokio::time::sleep(POLL).await;
        }
    }
}

/// The typed prompt of a step: its last user message that is not a generated reminder.
fn last_content(record: &Value) -> Option<&str> {
    record["params"]["messages"]
        .as_array()?
        .iter()
        .rev()
        .filter(|message| message["role"] == "user")
        .filter_map(|message| message["content"].as_str())
        .find(|content| !content.starts_with("[host notice]") && !content.starts_with("<harness "))
}

fn host_json(data: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(data.join("host.json")).ok()?).ok()
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
        std::fs::read_to_string(&self.out).unwrap_or_default()
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.err).unwrap_or_default()
    }

    fn running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    /// Sends one signal to this process's own pid. Only while it is still running.
    fn signal(&mut self, signal: libc::c_int) {
        assert!(
            self.running(),
            "{} already exited: {}",
            self.name,
            self.stderr()
        );
        assert_eq!(unsafe { libc::kill(self.pid as libc::pid_t, signal) }, 0);
    }

    async fn finish(&mut self) -> ExitStatus {
        let waited = tokio::time::timeout(WAIT, self.child.wait()).await;
        match waited {
            Ok(status) => status.unwrap(),
            Err(_) => panic!(
                "{} did not exit; stdout: {}; stderr: {}",
                self.name,
                self.stdout(),
                self.stderr()
            ),
        }
    }

    /// Waits until `ready` holds. Fails at once if this process exits first, with its stdout and stderr.
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

/// Starts the host with a long request, then waits until it publishes `host.json`
/// and its extension has received that request.
async fn start_host(lab: &Lab, data: &Path, workspace: &Path, prompt: &str) -> Proc {
    start_host_with(lab, data, workspace, prompt, &[]).await
}

/// `start_host` with extra environment on the host process only.
async fn start_host_with(
    lab: &Lab,
    data: &Path,
    workspace: &Path,
    prompt: &str,
    env: &[(&str, &str)],
) -> Proc {
    // Only the host's extension writes the exit marker, so standalone runs cannot satisfy its wait.
    let mut command = lab.command(data, workspace, "fake/code-model", &[], &["run", prompt]);
    extension_env(
        &mut command,
        "BP_FAKE_EXIT_MARKER",
        lab.exit_marker().to_str().unwrap(),
    );
    for (name, value) in env {
        extension_env(&mut command, name, value);
    }
    let mut host = Proc::spawn(lab.root(), "host", command);
    host.until("host.json", || host_json(data).is_some()).await;
    host.until(&format!("the extension to receive {prompt:?}"), || {
        lab.steps_for(prompt) > 0
    })
    .await;
    assert_eq!(host_json(data).unwrap()["pid"], host.pid as u64);
    host
}

fn extension_env(command: &mut Command, name: &str, value: &str) {
    command.arg("--set").arg(format!(
        "extensions.fake.env.{name}={}",
        toml::Value::String(value.into())
    ));
}

/// Stops a host this test started and waits until its extension has exited too.
async fn stop_host(lab: &Lab, host: &mut Proc) {
    host.signal(libc::SIGKILL);
    host.finish().await;
    lab.wait_for_extension_exit().await;
}

/// The last JSON line of a run's stdout, which is its `{"type":"result"}` record.
fn result_record(stdout: &str) -> Value {
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .rfind(|record| record["type"] == "result")
        .unwrap_or_else(|| panic!("no result record in: {stdout}"))
}

/// The probe reply's tool names, from `probe model=<m> tools=<a,b,...>`.
fn probe_tools(text: &str) -> Vec<&str> {
    text.split("tools=")
        .nth(1)
        .expect("probe reply lists tools")
        .split(',')
        .collect()
}

/// Replaces the parts of run output that differ between processes by design.
fn assignment(check: &str, rework: u32) -> String {
    let check = toml::Value::String(check.into());
    format!(
        "+++\ncategory = \"implement\"\ntags = [\"test\"]\nchecks = [{check}]\nrework = {rework}\nroute = {{ model = \"fake/code-model\" }}\n+++\nCreate done.txt.\n"
    )
}

#[tokio::test]
async fn host_and_attached_run_fit_a_one_mib_main_stack() {
    let lab = Lab::new();
    let data = lab.data();
    let workspace = lab.dir("small-stack");
    let limited = |prompt: &str| {
        let mut command = lab.command(&data, &workspace, "fake/code-model", &[], &["run", prompt]);
        // Apply the same main-thread limit as a Windows debug executable. The
        // limit survives exec; a small test thread alone would not constrain it.
        unsafe {
            command.pre_exec(|| {
                let limit = libc::rlimit {
                    rlim_cur: 1024 * 1024,
                    rlim_max: 1024 * 1024,
                };
                if libc::setrlimit(libc::RLIMIT_STACK, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command
    };
    let mut host = Proc::spawn(lab.root(), "small-stack-host", limited("sleep 6000"));
    host.until("host.json on a 1 MiB stack", || host_json(&data).is_some())
        .await;
    let mut client = Proc::spawn(lab.root(), "small-stack-client", limited("probe"));
    assert!(client.finish().await.success(), "{}", client.stderr());
    assert!(host.finish().await.success(), "{}", host.stderr());
    assert!(!data.join("host.json").exists());
}

#[tokio::test]
async fn second_run_attaches_with_its_own_model_config_and_workspace() {
    let lab = Lab::new();
    let data = lab.data();
    let a = lab.dir("a");
    let b = lab.dir("b");
    let mut host = start_host(&lab, &data, &a, "sleep 8000").await;
    let host_pid = host.pid as u64;

    // Two attached clients at once: a direct model on the default config, and
    // another direct model with its own workspace and `--read-only`.
    let mut plain = Proc::spawn(
        lab.root(),
        "plain",
        lab.command(&data, &a, "fake/direct-model", &[], &["run", "probe"]),
    );
    let mut read_only = Proc::spawn(
        lab.root(),
        "read-only",
        lab.command(
            &data,
            &b,
            "fake/direct-model",
            &["--read-only"],
            &["run", "probe"],
        ),
    );
    assert!(plain.finish().await.success(), "{}", plain.stderr());
    assert!(read_only.finish().await.success(), "{}", read_only.stderr());
    assert!(host.running(), "the host must outlive its clients");
    assert_eq!(
        host_json(&data).unwrap()["pid"],
        host_pid,
        "host pid unchanged"
    );

    let plain_text = result_record(&plain.stdout())["run"]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    let read_only_text = result_record(&read_only.stdout())["run"]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(plain_text.starts_with("probe model="), "{plain_text}");
    assert!(plain_text.contains("direct-model") && !plain_text.contains("code-model"));
    assert!(read_only_text.contains("direct-model"));
    assert!(probe_tools(&plain_text).contains(&"write"), "{plain_text}");
    assert!(
        !probe_tools(&read_only_text).contains(&"write"),
        "--read-only applies"
    );

    // Output parity with a standalone run of the same request on its own store.
    let standalone_data = lab.path("standalone");
    let mut standalone = Proc::spawn(
        lab.root(),
        "standalone",
        lab.command(
            &standalone_data,
            &a,
            "fake/direct-model",
            &[],
            &["run", "probe"],
        ),
    );
    assert!(
        standalone.finish().await.success(),
        "{}",
        standalone.stderr()
    );
    assert_eq!(
        normalize(&plain.stdout(), &data),
        normalize(&standalone.stdout(), &standalone_data),
        "attached stdout"
    );
    assert_eq!(
        normalize(&plain.stderr(), &data),
        normalize(&standalone.stderr(), &standalone_data),
        "attached stderr"
    );

    // The client's own workspace receives its tool effects; the host's does not.
    let write = "js await tools.exec({command:\"echo b > marker.txt\"})";
    let mut writer = Proc::spawn(
        lab.root(),
        "writer",
        lab.command(&data, &b, "fake/code-model", &[], &["run", write]),
    );
    assert!(writer.finish().await.success(), "{}", writer.stderr());
    assert!(b.join("marker.txt").exists(), "attached workspace");
    assert!(!a.join("marker.txt").exists(), "host workspace untouched");

    assert!(host.finish().await.success(), "{}", host.stderr());
    assert!(!data.join("host.json").exists());
}

#[tokio::test]
async fn root_workspace_of_an_attached_session_survives_catalog_rebuild() {
    let lab = Lab::new();
    let data = lab.data();
    let a = lab.dir("a");
    let b = lab.dir("b");
    let mut host = start_host(&lab, &data, &a, "sleep 3000").await;
    let first = "js await tools.exec({command:\"echo first > first.txt\"})";
    let mut root = Proc::spawn(
        lab.root(),
        "root",
        lab.command(&data, &b, "fake/code-model", &[], &["run", first]),
    );
    assert!(root.finish().await.success(), "{}", root.stderr());
    let session = result_record(&root.stdout())["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(b.join("first.txt").exists());
    assert!(host.finish().await.success(), "{}", host.stderr());

    let mut rebuild = Proc::spawn(
        lab.root(),
        "rebuild",
        lab.command(&data, &a, "fake/code-model", &[], &["catalog", "rebuild"]),
    );
    assert!(rebuild.finish().await.success(), "{}", rebuild.stderr());
    let counted: Value = serde_json::from_str(rebuild.stdout().trim()).unwrap();
    assert!(counted["sessions"].as_u64().unwrap() >= 2, "{counted}");

    let again = "js await tools.exec({command:\"echo again > again.txt\"})";
    let sub = ["run", "--session", session.as_str(), again];
    let mut resume = Proc::spawn(
        lab.root(),
        "resume",
        lab.command(&data, &b, "fake/code-model", &[], &sub),
    );
    assert!(resume.finish().await.success(), "{}", resume.stderr());
    assert!(
        b.join("again.txt").exists(),
        "rebuilt root keeps its workspace"
    );
    assert!(!a.join("again.txt").exists());
}

#[tokio::test]
async fn host_waits_for_an_attached_run_before_removing_its_metadata() {
    let lab = Lab::new();
    let data = lab.data();
    let a = lab.dir("a");
    let socket = data.join("host.sock");
    let mut host = start_host(&lab, &data, &a, "sleep 2000").await;
    let host_out = host.out.clone();
    let mut client = Proc::spawn(
        lab.root(),
        "client",
        lab.command(&data, &a, "fake/code-model", &[], &["run", "sleep 6000"]),
    );
    client
        .until("the client's step", || lab.steps_for("sleep 6000") > 0)
        .await;

    // The host's own run finishes first and its result is printed; the client is still attached.
    host.until("the host's own result", || {
        std::fs::read_to_string(&host_out)
            .unwrap_or_default()
            .contains("\"type\":\"result\"")
    })
    .await;
    assert!(
        client.running(),
        "client still attached: {}",
        client.stderr()
    );
    assert!(host.running(), "host waits for its attached client");
    assert!(
        data.join("host.json").exists(),
        "metadata kept while attached"
    );
    assert!(socket.exists(), "socket kept while attached");

    assert!(client.finish().await.success(), "{}", client.stderr());
    assert!(host.finish().await.success(), "{}", host.stderr());
    assert!(
        !data.join("host.json").exists(),
        "host.json removed at exit"
    );
    assert!(!socket.exists(), "socket removed at exit");
}

#[tokio::test]
async fn ctrl_c_and_disconnect_cancel_only_their_own_session() {
    let lab = Lab::new();
    let data = lab.data();
    let a = lab.dir("a");
    let mut host = start_host(&lab, &data, &a, "sleep 60000").await;

    // Ctrl-C reaches only this client, which sends the cancel request over the socket.
    let mut interrupted = Proc::spawn(
        lab.root(),
        "interrupted",
        lab.command(&data, &a, "fake/code-model", &[], &["run", "hang one"]),
    );
    // Readiness for SIGINT: the client registers its Ctrl-C listener before it
    // writes `attach.submit`, so the host having produced this step proves the
    // listener already exists and will not be lost.
    interrupted
        .until("the hang step", || lab.steps_for("hang one") == 1)
        .await;
    let interrupted_id = lab.step_id("hang one");
    interrupted.signal(libc::SIGINT);
    assert!(!interrupted.finish().await.success());
    assert_eq!(
        result_record(&interrupted.stdout())["run"]["status"],
        "cancelled"
    );
    host.until("the cancel to reach the extension", || {
        lab.cancel_targets().contains(&interrupted_id)
    })
    .await;

    // A client that disappears without a cancel is treated the same way by the host.
    let mut dropped = Proc::spawn(
        lab.root(),
        "dropped",
        lab.command(&data, &a, "fake/code-model", &[], &["run", "hang two"]),
    );
    dropped
        .until("the second hang step", || lab.steps_for("hang two") == 1)
        .await;
    let dropped_id = lab.step_id("hang two");
    dropped.signal(libc::SIGKILL);
    assert!(!dropped.finish().await.success());
    host.until("the cancel for the disconnected client", || {
        lab.cancel_targets().contains(&dropped_id)
    })
    .await;

    // The host and its other sessions keep working.
    let mut probe = Proc::spawn(
        lab.root(),
        "probe",
        lab.command(&data, &a, "fake/code-model", &[], &["run", "probe"]),
    );
    assert!(probe.finish().await.success(), "{}", probe.stderr());
    assert!(host.running(), "host survives client cancellation");
    assert_eq!(host_json(&data).unwrap()["pid"], host.pid as u64);
    let probe_id = lab.step_id("probe");
    let targets = lab.cancel_targets();
    assert!(!targets.contains(&probe_id), "the probe was not cancelled");
    assert_eq!(
        lab.steps_for("hang one"),
        1,
        "no replay of the cancelled step"
    );

    stop_host(&lab, &mut host).await;
}

#[tokio::test]
async fn killing_the_host_interrupts_attached_clients_without_replaying_them() {
    let lab = Lab::new();
    let data = lab.data();
    let a = lab.dir("a");
    let mut host = start_host(&lab, &data, &a, "hang host").await;
    let mut client = Proc::spawn(
        lab.root(),
        "client",
        lab.command(&data, &a, "fake/code-model", &[], &["run", "hang client"]),
    );
    client
        .until("the client's step", || lab.steps_for("hang client") == 1)
        .await;

    host.signal(libc::SIGKILL);
    assert!(!host.finish().await.success());

    assert!(!client.finish().await.success(), "{}", client.stderr());
    let result = result_record(&client.stdout());
    assert_eq!(
        result["run"]["status"],
        "interrupted",
        "{}",
        client.stdout()
    );
    assert_eq!(
        lab.steps_for("hang client"),
        1,
        "the interrupted step was not replayed"
    );
    lab.wait_for_extension_exit().await;
}

#[tokio::test]
async fn non_attaching_command_on_an_owned_data_dir_names_the_host_pid() {
    let lab = Lab::new();
    let data = lab.data();
    let a = lab.dir("a");
    let mut host = start_host(&lab, &data, &a, "sleep 60000").await;

    let mut sessions = Proc::spawn(
        lab.root(),
        "sessions",
        lab.command(&data, &a, "fake/code-model", &[], &["sessions"]),
    );
    assert!(!sessions.finish().await.success());
    let stderr = sessions.stderr();
    assert!(
        stderr.contains(&format!("owned by host pid {}", host.pid)),
        "{stderr}"
    );

    stop_host(&lab, &mut host).await;
}

/// Explicit extension configuration applies independently to each request.
#[tokio::test]
async fn attached_extensions_keep_their_configured_environment() {
    let lab = Lab::new();
    let data = lab.data();
    let a = lab.dir("a");
    // The host's extension answers `host-reply` after a delay, which keeps the host up.
    let mut host = start_host_with(
        &lab,
        &data,
        &a,
        "sleep 8000",
        &[
            ("BP_FAKE_REPLY", "host-reply"),
            ("BP_FAKE_REPLY_DELAY_MS", "8000"),
        ],
    )
    .await;

    let mut with_reply = lab.command(&data, &a, "fake/code-model", &[], &["run", "ask"]);
    extension_env(&mut with_reply, "BP_FAKE_REPLY", "client-reply");
    with_reply.env("BP_FAKE_REPLY", "unreferenced-client-value");
    let mut client = Proc::spawn(lab.root(), "client", with_reply);
    let mut without_reply = lab.command(&data, &a, "fake/code-model", &[], &["run", "ask"]);
    without_reply.env_remove("BP_FAKE_REPLY");
    let mut plain = Proc::spawn(lab.root(), "plain", without_reply);

    assert!(client.finish().await.success(), "{}", client.stderr());
    assert!(plain.finish().await.success(), "{}", plain.stderr());
    assert_eq!(
        result_record(&client.stdout())["run"]["text"],
        "client-reply"
    );
    // The fake's default reply, since neither the host's nor the client's reply variable applies.
    assert_eq!(result_record(&plain.stdout())["run"]["text"], "ok");

    assert!(host.finish().await.success(), "{}", host.stderr());
    assert_eq!(result_record(&host.stdout())["run"]["text"], "host-reply");
}

#[tokio::test]
async fn endpoint_is_user_only_and_the_host_listens_on_no_network_port() {
    let lab = Lab::new();
    let data = lab.data();
    let a = lab.dir("a");
    let mut host = start_host(&lab, &data, &a, "sleep 60000").await;

    let metadata = host_json(&data).unwrap();
    let keys: HashSet<&str> = metadata
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, HashSet::from(["pid", "endpoint", "protocol_version"]));
    assert_eq!(metadata["protocol_version"], 3);
    assert_eq!(metadata["pid"].as_u64(), Some(host.pid as u64));

    let endpoint = PathBuf::from(metadata["endpoint"].as_str().unwrap());
    assert_eq!(endpoint.file_name().unwrap(), "host.sock");
    let socket = std::fs::metadata(&endpoint).unwrap();
    assert!(socket.file_type().is_socket(), "endpoint is a Unix socket");
    assert_eq!(
        socket.permissions().mode() & 0o777,
        0o600,
        "socket is user-only"
    );
    let directory = std::fs::metadata(&data).unwrap();
    assert_eq!(
        directory.permissions().mode() & 0o777,
        0o700,
        "directory is user-only"
    );

    assert_eq!(
        proc_sockets::tcp_listeners(host.pid),
        Vec::<String>::new(),
        "no TCP listener"
    );

    stop_host(&lab, &mut host).await;
}

#[path = "host_attach/stdio_shutdown.rs"]
mod stdio_shutdown;
