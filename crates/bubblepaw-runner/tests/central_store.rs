//! Without `--data-dir`, every workspace uses one central store under the local
//! data home, and legacy per-workspace stores beside it are imported once.
//! The sandbox sets LOCALAPPDATA, which takes precedence on every platform, so
//! no test touches the user's real data directory.
use bubblepaw_core::{QuotaReading, QuotaSource, RequestUsage, Store};
use bubblepaw_runner::delegate::{
    ledger::{AttemptRecord, Ledger},
    metrics::{AttemptMetrics, CallRow},
    pricing::TokenClasses,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

const LEGACY: &str = "5f1e0c3b9a7d2e4f6a8b0c1d3e5f7a9b1c3d5e7f9a0b2c4d6e8f0a1b3c5d7e9f";

struct Sandbox {
    root: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }

    fn workspace(&self, name: &str) -> PathBuf {
        let path = self.root.path().join(name);
        std::fs::create_dir_all(&path).unwrap();
        path.canonicalize().unwrap()
    }

    fn base(&self) -> PathBuf {
        self.root.path().join("local").join("BubblePaw-Core")
    }

    /// The store folder before the rename to `BubblePaw-Core`.
    fn previous(&self) -> PathBuf {
        self.root
            .path()
            .join("local")
            .join("BubblePaw-base")
            .join("store")
    }

    fn central(&self) -> PathBuf {
        self.base().join("store")
    }

    fn command(&self, workspace: &Path, args: &[&str]) -> Command {
        let home = self.root.path().join("home");
        let mut command = Command::new(env!("CARGO_BIN_EXE_bubblepaw"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("BUBBLEPAW_") {
                command.env_remove(key);
            }
        }
        command
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENROUTER_API_KEY")
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("LOCALAPPDATA", self.root.path().join("local"))
            .env("APPDATA", home.join("roaming"))
            .env("XDG_DATA_HOME", self.root.path().join("xdg"))
            .env("CODEX_HOME", home.join("codex"))
            .current_dir(workspace)
            .args(["--provider", "demo", "--json", "--workspace"])
            .arg(workspace)
            .args(args);
        command
    }

    fn cli(&self, workspace: &Path, args: &[&str]) -> Output {
        let output = self.command(workspace, args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn session_of(output: &Output) -> String {
    stdout(output)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|record| record["type"] == "result")
        .and_then(|record| record["session_id"].as_str().map(str::to_owned))
        .expect("run result")
}

fn digest_tree(root: &Path) -> BTreeMap<PathBuf, String> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    format!("{:x}", Sha256::digest(std::fs::read(&path).unwrap())),
                );
            }
        }
    }
    files
}

#[test]
fn workspaces_without_data_dir_share_the_central_store_and_keep_their_workspaces() {
    let sandbox = Sandbox::new();
    let first = sandbox.workspace("first");
    let second = sandbox.workspace("second");
    let a = session_of(&sandbox.cli(&first, &["run", "hello from the first workspace"]));
    let b = session_of(&sandbox.cli(&second, &["run", "hello from the second workspace"]));
    assert_ne!(a, b);
    // One store, named `store`; no per-workspace hash directory is created.
    let entries: Vec<_> = std::fs::read_dir(sandbox.base())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, ["store"]);
    let listed: Vec<Value> =
        serde_json::from_str(&stdout(&sandbox.cli(&second, &["sessions"]))).unwrap();
    let ids: Vec<_> = listed.iter().filter_map(|s| s["id"].as_str()).collect();
    assert!(
        ids.contains(&a.as_str()) && ids.contains(&b.as_str()),
        "{ids:?}"
    );
    let store = Store::open_data_dir(sandbox.central()).unwrap();
    assert_eq!(
        store.session_workspace(&a).unwrap().as_deref(),
        Some(first.as_path())
    );
    assert_eq!(
        store.session_workspace(&b).unwrap().as_deref(),
        Some(second.as_path())
    );
}

#[test]
fn a_previous_default_store_is_imported_once_and_left_unchanged() {
    let sandbox = Sandbox::new();
    let workspace = sandbox.workspace("previous-workspace");
    let previous = sandbox.previous();
    let store = Store::open_data_dir(&previous).unwrap();
    store.bind_workspace(&workspace).unwrap();
    let session = store.create_session("previous default chat").unwrap();
    drop(store);
    let before = digest_tree(&previous);
    let first = sandbox.cli(&workspace, &["sessions"]);
    assert!(
        stderr(&first).contains("Imported legacy store"),
        "{}",
        stderr(&first)
    );
    let listed: Vec<Value> = serde_json::from_str(&stdout(&first)).unwrap();
    assert!(listed.iter().any(|s| s["id"] == session.id), "{listed:?}");
    let second = sandbox.cli(&workspace, &["sessions"]);
    assert!(
        !stderr(&second).contains("Imported legacy store"),
        "{}",
        stderr(&second)
    );
    assert_eq!(digest_tree(&previous), before);
}

fn attempt(id: &str, workspace: &Path) -> AttemptRecord {
    AttemptRecord {
        id: id.into(),
        delegation_id: format!("delegation-{id}"),
        parent_id: None,
        started_at: "2026-10-19T00:00:00Z".into(),
        ended_at: "2026-10-19T00:00:01Z".into(),
        assignment_hash: "h".into(),
        category: "legacy-category".into(),
        tags: vec![],
        workspace: workspace.to_string_lossy().into_owned(),
        repo: None,
        assignment_tokens: 1,
        check_count: 0,
        route_class: None,
        model: "m".into(),
        route_reason: "r".into(),
        period: "p".into(),
        rework_index: 0,
        session_id: "s".into(),
        run_status: "completed".into(),
        report_status: None,
        final_status: "accepted".into(),
        accepted: true,
        checks: json!([]),
        false_greens: 0,
        tokens: TokenClasses {
            uncached_input: 10,
            cache_read: 0,
            cache_write: 0,
            output: 5,
            reasoning: None,
        },
        cost_usd: None,
        price_table: "t".into(),
        wall_ms: 1,
        check_ms: 0,
        metrics: AttemptMetrics::default(),
        calls: vec![CallRow {
            seq: 0,
            model: "m".into(),
            provider_route: None,
            account: None,
            fallback_reason: None,
            tokens: None,
            ttft_ms: None,
            generation_ms: None,
            tokens_per_s: None,
            outcome: "ok".into(),
            expected_cacheable_tokens: None,
            unexplained_miss_tokens: None,
            prefix_break: None,
            wire_bytes: None,
            at: None,
        }],
    }
}

#[test]
fn reports_over_the_central_store_include_legacy_rows_exactly_once() {
    let sandbox = Sandbox::new();
    let workspace = sandbox.workspace("legacy-workspace");
    let legacy = sandbox.base().join(LEGACY);
    let store = Store::open_data_dir(&legacy).unwrap();
    store.bind_workspace(&workspace).unwrap();
    let session = store.create_session("legacy chat").unwrap();
    let mut usage = RequestUsage::new("legacy-provider", "legacy-model");
    usage.session_id.clone_from(&session.id);
    usage.request_id = "legacy-request-0001".into();
    store.queue_request_usage(usage).unwrap();
    store.flush_request_usage(&session.id).unwrap();
    store
        .record_quota(&QuotaReading {
            provider: "p".into(),
            account: "legacy-account".into(),
            window: "weekly".into(),
            used_percent: 42.0,
            resets_at: None,
            observed_at: "2026-10-20T00:00:00Z".into(),
            source: QuotaSource::ResponseHeaders,
            balance: None,
        })
        .unwrap();
    drop(store);
    let mut ledger = Ledger::open(&legacy.join("delegations.db")).unwrap();
    ledger
        .record(&attempt("legacy-attempt-1", &workspace))
        .unwrap();
    ledger
        .record(&attempt("legacy-attempt-2", &workspace))
        .unwrap();
    drop(ledger);
    let before = digest_tree(&legacy);

    let elsewhere = sandbox.workspace("elsewhere");
    let first = sandbox.cli(&elsewhere, &["sessions"]);
    assert!(
        stderr(&first).contains("Imported legacy store") && stderr(&first).contains("1 chats"),
        "{}",
        stderr(&first)
    );
    let reports = |label: &str| {
        let usage = stdout(&sandbox.cli(&elsewhere, &["usage", "export"]));
        let quota = stdout(&sandbox.cli(&elsewhere, &["quota", "export"]));
        let delegations: Value = serde_json::from_str(&stdout(
            &sandbox.cli(&elsewhere, &["delegations", "report"]),
        ))
        .unwrap();
        assert_eq!(
            usage.matches("legacy-request-0001").count(),
            1,
            "{label}: {usage}"
        );
        assert_eq!(
            quota.matches("legacy-account").count(),
            1,
            "{label}: {quota}"
        );
        let group = delegations["groups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|group| group["category"] == "legacy-category")
            .unwrap_or_else(|| panic!("{label}: {delegations}"))
            .clone();
        assert_eq!(group["attempts"], 2, "{label}: {group}");
        assert_eq!(group["delegations"], 2, "{label}: {group}");
        (usage, quota, delegations)
    };
    let imported = reports("after import");
    let listed: Vec<Value> = serde_json::from_str(&stdout(&first)).unwrap();
    assert!(listed.iter().any(|s| s["id"] == session.id.as_str()));
    let store = Store::open_data_dir(sandbox.central()).unwrap();
    assert_eq!(
        store.session_workspace(&session.id).unwrap().as_deref(),
        Some(workspace.as_path())
    );
    drop(store);

    // A second start imports nothing; every report is unchanged.
    let second = sandbox.cli(&elsewhere, &["sessions"]);
    assert!(
        !stderr(&second).contains("legacy store"),
        "{}",
        stderr(&second)
    );
    assert_eq!(reports("after restart"), imported);
    assert_eq!(digest_tree(&legacy), before, "legacy store changed");
}

#[test]
fn a_client_attaching_during_the_import_waits_for_the_host() {
    let sandbox = Sandbox::new();
    let workspace = sandbox.workspace("project");
    let legacy = sandbox.base().join(LEGACY);
    let store = Store::open_data_dir(&legacy).unwrap();
    store.bind_workspace(&workspace).unwrap();
    store.create_session("legacy chat").unwrap();
    drop(store);
    let gate = sandbox.root.path().join("release-import");
    let spawn = |prompt: &str, gated: bool| {
        let mut command = sandbox.command(&workspace, &["run", prompt]);
        if gated {
            command.env("BPBASE_TEST_IMPORT_GATE", &gate);
        }
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let mut host = spawn("hello from the host", true);
    let published = sandbox.central().join("host.json");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !published.exists() {
        assert!(Instant::now() < deadline, "host never published");
        if host.try_wait().unwrap().is_some() {
            let output = host.wait_with_output().unwrap();
            panic!("host exited early: {}", stderr(&output));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut client = spawn("hello from the client", false);
    // Longer than the five-second attach wait: the client must still wait.
    std::thread::sleep(Duration::from_secs(7));
    assert!(client.try_wait().unwrap().is_none(), "client gave up");
    std::fs::write(&gate, b"go").unwrap();
    let host = host.wait_with_output().unwrap();
    let client = client.wait_with_output().unwrap();
    assert!(host.status.success(), "host: {}", stderr(&host));
    assert!(client.status.success(), "client: {}", stderr(&client));
    assert!(
        stderr(&host).contains("Imported legacy store"),
        "{}",
        stderr(&host)
    );
    assert_eq!(
        stderr(&client)
            .matches("waiting for the host to finish importing legacy stores")
            .count(),
        1,
        "{}",
        stderr(&client)
    );
    assert!(!stderr(&client).contains("Imported legacy store"));
    let host_session = session_of(&host);
    let client_session = session_of(&client);
    assert_ne!(host_session, client_session);
    let listed: Vec<Value> =
        serde_json::from_str(&stdout(&sandbox.cli(&workspace, &["sessions"]))).unwrap();
    let ids: Vec<_> = listed.iter().filter_map(|s| s["id"].as_str()).collect();
    assert!(
        ids.contains(&host_session.as_str()) && ids.contains(&client_session.as_str()),
        "{ids:?}"
    );
    assert_eq!(listed.len(), 3, "{listed:?}");
}

#[test]
fn a_client_waiting_on_a_one_shot_import_runs_afterwards() {
    let sandbox = Sandbox::new();
    let workspace = sandbox.workspace("project");
    let legacy = sandbox.base().join(LEGACY);
    let store = Store::open_data_dir(&legacy).unwrap();
    store.bind_workspace(&workspace).unwrap();
    store.create_session("legacy chat").unwrap();
    drop(store);
    let gate = sandbox.root.path().join("release-import");
    let mut host = sandbox
        .command(&workspace, &["sessions"])
        .env("BPBASE_TEST_IMPORT_GATE", &gate)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let published = sandbox.central().join("host.json");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !published.exists() {
        if host.try_wait().unwrap().is_some() {
            let output = host.wait_with_output().unwrap();
            panic!("host exited early: {}", stderr(&output));
        }
        assert!(Instant::now() < deadline, "host never published");
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut client = sandbox
        .command(&workspace, &["run", "hello after the import"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Open the gate only once the client has printed the import notice.
    let notice = "waiting for the host to finish importing legacy stores";
    let (noticed, seen) = std::sync::mpsc::channel();
    let reader = std::io::BufReader::new(client.stderr.take().unwrap());
    let client_stderr = std::thread::spawn(move || {
        let mut text = String::new();
        for line in std::io::BufRead::lines(reader).map_while(Result::ok) {
            if line.contains(notice) {
                let _ = noticed.send(());
            }
            text.push_str(&line);
            text.push('\n');
        }
        text
    });
    let attached = seen.recv_timeout(Duration::from_secs(60));
    std::fs::write(&gate, b"go").unwrap();
    assert!(host.wait().unwrap().success());
    let client = client.wait_with_output().unwrap();
    let client_stderr = client_stderr.join().unwrap();
    assert!(attached.is_ok(), "no import notice: {client_stderr}");
    assert!(client.status.success(), "client: {client_stderr}");
    assert_eq!(client_stderr.matches(notice).count(), 1, "{client_stderr}");
    session_of(&client);
}
