//! `bubblepaw delegate` with scripted models: harness-run checks, rework, failed
//! resolutions, the ledger's one transaction per attempt and the result record.
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelInfo, ModelRequest, ModelResponse, Provider,
    Role, Store, TextCallback, ToolCall, ToolRegistry,
};
use bubblepaw_runner::{
    artifacts::ArtifactStore,
    codemode::CodeMode,
    delegate::{
        self, DelegateRun, assignment::Assignment, ledger::Ledger, metrics::Collector,
        routing::RouteChoice,
    },
    provider::{ScriptedProvider, ScriptedStep},
    tools::register_native_tools,
};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

#[cfg(windows)]
const CHECK: &str = "if (!(Test-Path done.txt)) { Write-Output 'done.txt missing'; exit 1 }";
#[cfg(not(windows))]
const CHECK: &str = "test -f done.txt || { echo 'done.txt missing'; exit 1; }";

fn report(status: &str, claims: Value, questions: Value) -> ScriptedStep {
    js(&format!(
        "await tools.submit_result({});",
        json!({
            "status": status,
            "summary": format!("worker says {status}"),
            "checks_claimed": claims,
            "open_questions": questions,
        })
    ))
}
fn js(code: &str) -> ScriptedStep {
    ScriptedStep::Calls {
        calls: vec![ToolCall {
            id: uuid::Uuid::new_v4().to_string(),
            name: "js".into(),
            arguments: json!({ "code": code }),
        }],
    }
}

struct Fixture {
    harness: Arc<Harness>,
    store: Store,
    collector: Arc<Collector>,
    _processes: Box<dyn std::any::Any>,
}

fn fixture(root: &Path, steps: Vec<ScriptedStep>) -> Fixture {
    fixture_with_provider(
        root,
        Arc::new(ScriptedProvider::with_steps(steps, "unused")),
    )
}

fn fixture_with_provider(root: &Path, provider: Arc<dyn Provider>) -> Fixture {
    let store = Store::in_memory().unwrap();
    let mut tools = ToolRegistry::new();
    let processes = register_native_tools(&mut tools, root, false).unwrap();
    let repl = Arc::new(
        CodeMode::new(
            root.to_owned(),
            Arc::new(ArtifactStore::new(root, root.join(".artifacts")).unwrap()),
        )
        .unwrap(),
    );
    repl.register(&mut tools).unwrap();
    let collector = Collector::new(None);
    let harness = Arc::new(Harness::new(
        store.clone(),
        provider,
        tools,
        HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            observation: Some(collector.observation()),
            ..Default::default()
        },
    ));
    Fixture {
        harness,
        store,
        collector,
        _processes: Box::new(processes),
    }
}

fn assignment(rework: u32) -> Assignment {
    let check = toml::Value::String(CHECK.into());
    Assignment::parse(&format!(
        "+++\ncategory = \"implement\"\ntags = [\"test\"]\nchecks = [{check}]\nrework = {rework}\nroute = {{ class = \"small\" }}\n+++\nCreate done.txt.\n"
    ))
    .unwrap()
}

fn route() -> RouteChoice {
    RouteChoice {
        model: "opencode/deepseek-v4.1-flash".into(),
        class: Some("small".into()),
        reason: "default".into(),
    }
}

/// Commit frames in a WAL file: each committed transaction ends with one.
fn wal_commits(db: &Path) -> usize {
    let wal = std::fs::read(db.with_extension("db-wal")).unwrap_or_default();
    if wal.len() < 32 {
        return 0;
    }
    let page = u32::from_be_bytes(wal[8..12].try_into().unwrap()) as usize;
    wal[32..]
        .chunks(24 + page)
        .filter(|frame| frame.len() == 24 + page)
        .filter(|frame| u32::from_be_bytes(frame[4..8].try_into().unwrap()) != 0)
        .count()
}

fn wal_ledger(path: &Path) -> (Ledger, rusqlite::Connection) {
    let keep = rusqlite::Connection::open(path).unwrap();
    keep.pragma_update(None, "journal_mode", "WAL").unwrap();
    (Ledger::open(path).unwrap(), keep)
}

#[tokio::test]
async fn failed_check_is_reworked_in_the_same_session_and_passes() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("ws");
    std::fs::create_dir(&workspace).unwrap();
    let steps = vec![
        // Attempt 1 claims a pass without doing the work: a false green.
        report(
            "done",
            json!([{"command": CHECK, "passed": true}]),
            json!([]),
        ),
        // Attempt 2 creates the file, runs the check itself, then reports.
        js(&format!(
            "await tools.exec({{command:{}}}); const r = await tools.exec({{command:{}}}); text(String(r.exitCode));",
            json!("echo ok > done.txt"),
            json!(CHECK)
        )),
        report(
            "done",
            json!([{"command": CHECK, "passed": true}]),
            json!([]),
        ),
    ];
    let f = fixture(&workspace, steps);
    let db = root.path().join("delegations.db");
    let (mut ledger, _keep) = wal_ledger(&db);
    let before = wal_commits(&db);
    let result = delegate::run(
        f.harness.clone(),
        &f.store,
        &mut ledger,
        DelegateRun {
            assignment: assignment(1),
            route: route(),
            workspace: workspace.clone(),
            title: "delegate test".into(),
            strategy: Default::default(),
            events: None,
            collector: Some(f.collector.clone()),
        },
    )
    .await
    .unwrap();
    assert_eq!(result.status, "accepted", "{result:#?}");
    assert_eq!(result.attempts, 2);
    assert!(result.failing.is_empty());
    assert_eq!(result.checks.len(), 1);
    assert!(result.checks[0].starts_with("pass "), "{:?}", result.checks);
    assert_eq!(result.false_greens, 0, "the final attempt's claim held");
    assert_eq!(result.changed_files, Vec::<String>::new(), "not a git repo");
    assert_eq!(wal_commits(&db) - before, 2, "one transaction per attempt");

    // Only one worker session, resumed for the rework.
    let conn = rusqlite::Connection::open(&db).unwrap();
    let rows: Vec<(String, i64, String, i64, String, i64)> = conn
        .prepare("SELECT session_id, rework_index, final_status, false_greens, route_reason, done_without_check FROM delegation_attempts ORDER BY rework_index")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, rows[1].0);
    assert_eq!(rows[0].0, result.session_id);
    assert_eq!((rows[0].1, rows[0].2.as_str(), rows[0].3), (0, "rework", 1));
    assert_eq!(
        (rows[1].1, rows[1].2.as_str(), rows[1].3),
        (1, "accepted", 0)
    );
    assert_eq!(rows[0].4, "default");
    assert_eq!(
        (rows[0].5, rows[1].5),
        (1, 0),
        "attempt 1 claimed done with no check run"
    );
    // Call rows came from the observation records; the scripted model reports no usage.
    let calls: i64 = conn
        .query_row("SELECT COUNT(*) FROM delegation_calls", [], |r| r.get(0))
        .unwrap();
    assert_eq!(calls, 3, "one request in attempt 1, two in attempt 2");
    let (execs, ms_first): (String, Option<i64>) = conn
        .query_row(
            "SELECT metrics, ms_to_first_pass FROM delegation_attempts WHERE rework_index = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let metrics: Value = serde_json::from_str(&execs).unwrap();
    assert_eq!(metrics["tool_calls_by_name"]["exec"], 2);
    assert!(ms_first.is_some());

    let report = ledger.report().unwrap();
    let group = &report["groups"][0];
    assert_eq!(group["category"], "implement");
    assert_eq!(group["class"], "small");
    assert_eq!(group["delegations"], 1);
    assert_eq!(group["accepted"], 1);
    assert_eq!(group["attempts"], 2);
}

#[tokio::test]
async fn exhausted_rework_returns_a_failed_resolution() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("ws");
    std::fs::create_dir(&workspace).unwrap();
    let steps = vec![
        report("done", json!([]), json!([])),
        report(
            "blocked",
            json!([{"command": CHECK, "passed": false}]),
            json!(["Where should done.txt come from?"]),
        ),
    ];
    let f = fixture(&workspace, steps);
    let db = root.path().join("delegations.db");
    let mut ledger = Ledger::open(&db).unwrap();
    let result = delegate::run(
        f.harness.clone(),
        &f.store,
        &mut ledger,
        DelegateRun {
            assignment: assignment(1),
            route: route(),
            workspace: workspace.clone(),
            title: "delegate test".into(),
            strategy: Default::default(),
            events: None,
            collector: Some(f.collector.clone()),
        },
    )
    .await
    .unwrap();
    assert_eq!(result.status, "failed");
    assert_eq!(result.attempts, 2);
    assert_eq!(result.report_status.as_deref(), Some("blocked"));
    assert_eq!(result.open_questions, ["Where should done.txt come from?"]);
    assert_eq!(result.failing.len(), 1);
    assert!(
        result.failing[0].tail.contains("done.txt missing"),
        "{:?}",
        result.failing
    );
    assert!(result.checks[0].starts_with("fail (exit 1)"));

    // The compact record's shape is what the dispatcher reads.
    let record = serde_json::to_value(&result).unwrap();
    let keys: Vec<_> = record.as_object().unwrap().keys().cloned().collect();
    for key in [
        "type",
        "status",
        "ledger_id",
        "delegation_id",
        "session_id",
        "model",
        "class",
        "route_reason",
        "attempts",
        "checks",
        "failing",
        "changed_files",
        "diff_stat",
        "report_status",
        "summary",
        "open_questions",
        "false_greens",
        "cost_usd",
        "price_table",
        "error",
    ] {
        assert!(keys.iter().any(|k| k == key), "missing {key} in {keys:?}");
    }
    assert_eq!(keys.len(), 20, "{keys:?}");
    assert_eq!(record["type"], "delegation");
    assert_eq!(record["failing"][0]["exit_code"], 1);
    assert_eq!(record["summary"], "worker says blocked");
    // The scripted model reports no usage; zero tokens price at zero.
    assert_eq!(record["cost_usd"], 0.0);
    assert_eq!(record["price_table"], "2026-10-19");

    // A follow-up must name an attempt that exists, and records its reason.
    let check = toml::Value::String(CHECK.into());
    let follow = Assignment::parse(&format!(
        "+++\ncategory = \"implement\"\nchecks = [{check}]\nroute = {{ class = \"small\" }}\nparent = \"{}\"\nreason = \"escalate\"\n+++\nRetry.\n",
        result.ledger_id
    ))
    .unwrap();
    assert!(ledger.attempt_exists(&result.ledger_id).unwrap());
    let bogus = Assignment::parse(
        "+++\ncategory = \"implement\"\nroute = { class = \"small\" }\nparent = \"nope\"\nreason = \"reframe\"\n+++\nRetry.\n",
    )
    .unwrap();
    let g = fixture(&workspace, vec![]);
    let error = delegate::run(
        g.harness.clone(),
        &g.store,
        &mut ledger,
        DelegateRun {
            assignment: bogus,
            route: route(),
            workspace: workspace.clone(),
            title: "t".into(),
            strategy: Default::default(),
            events: None,
            collector: None,
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("parent ledger id"));
    assert_eq!(follow.parent.unwrap().1.as_str(), "escalate");
}

#[tokio::test]
async fn cli_delegate_writes_the_ledger_and_reports() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let workspace = root.path().join("ws");
    std::fs::create_dir(&workspace)?;
    let file = root.path().join("task.md");
    std::fs::write(
        &file,
        "+++\ncategory = \"research\"\nchecks = [\"exit 0\"]\nroute = { model = \"demo/scripted\" }\n+++\nList the files.\n",
    )?;
    let data = root.path().join("data");
    let run = |args: &[&str]| {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_bubblepaw"));
        command
            .args([
                "--provider",
                "demo",
                "--read-only",
                "--no-auto-summarize",
                "--workspace",
            ])
            .arg(&workspace)
            .arg("--data-dir")
            .arg(&data)
            .args(args)
            .env_remove("OPENROUTER_API_KEY")
            .env_remove("OPENAI_API_KEY");
        command.output()
    };
    let output = run(&["--json", "delegate", file.to_str().unwrap(), "--seed", "1"])?;
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let last: Value = serde_json::from_str(stdout.lines().last().unwrap_or("{}"))
        .map_err(|e| anyhow::anyhow!("{e}: {stdout}\n{stderr}"))?;
    assert_eq!(last["type"], "delegation", "{stdout}\n{stderr}");
    assert_eq!(last["model"], "demo/scripted");
    assert_eq!(last["route_reason"], "named");
    // The demo's prose reply is not a valid final report, so the run fails.
    assert_eq!(last["status"], "failed", "{stdout}\n{stderr}");
    assert!(!output.status.success());
    assert!(stdout.lines().count() > 1, "events precede the record");
    let report = run(&["delegations", "report"])?;
    assert!(
        report.status.success(),
        "{}",
        String::from_utf8_lossy(&report.stderr)
    );
    let report: Value = serde_json::from_slice(&report.stdout)?;
    assert_eq!(report["groups"][0]["category"], "research");
    assert_eq!(report["groups"][0]["class"], "named");
    assert_eq!(report["groups"][0]["accepted"], 0);
    // Unknown categories are rejected before anything runs.
    std::fs::write(
        &file,
        "+++\ncategory = \"dance\"\nroute = { class = \"small\" }\n+++\nx\n",
    )?;
    let rejected = run(&["delegate", file.to_str().unwrap()])?;
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("unknown assignment category"));
    Ok(())
}

fn read_first_assignment(rework: u32, read_first: &str) -> Assignment {
    let check = toml::Value::String(CHECK.into());
    Assignment::parse(&format!(
        "+++\ncategory = \"implement\"\nchecks = [{check}]\nrework = {rework}\nread_first = [{read_first}]\nroute = {{ class = \"small\" }}\n+++\nCreate done.txt.\n"
    ))
    .unwrap()
}

#[tokio::test]
async fn read_first_opens_the_first_message_and_a_rework_resume_does_not_repeat_it() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("ws");
    std::fs::create_dir_all(workspace.join("notes")).unwrap();
    std::fs::write(
        workspace.join("notes/brief.md"),
        "line one\nline two\nline three\n",
    )
    .unwrap();
    // The same two-attempt script as the rework test: attempt 1 is a false green.
    let steps = vec![
        report(
            "done",
            json!([{"command": CHECK, "passed": true}]),
            json!([]),
        ),
        js(&format!(
            "await tools.exec({{command:{}}}); const r = await tools.exec({{command:{}}}); text(String(r.exitCode));",
            json!("echo ok > done.txt"),
            json!(CHECK)
        )),
        report(
            "done",
            json!([{"command": CHECK, "passed": true}]),
            json!([]),
        ),
    ];
    let f = fixture(&workspace, steps);
    let db = root.path().join("delegations.db");
    let mut ledger = Ledger::open(&db).unwrap();
    let result = delegate::run(
        f.harness.clone(),
        &f.store,
        &mut ledger,
        DelegateRun {
            assignment: read_first_assignment(1, "\"notes/brief.md:2-3\""),
            route: route(),
            workspace: workspace.clone(),
            title: "delegate test".into(),
            strategy: Default::default(),
            events: None,
            collector: Some(f.collector.clone()),
        },
    )
    .await
    .unwrap();
    assert_eq!(result.status, "accepted", "{result:#?}");
    assert_eq!(result.attempts, 2);

    let users: Vec<String> = f
        .store
        .messages(&result.session_id)
        .unwrap()
        .into_iter()
        .filter(|m| m.role == Role::User)
        .map(|m| m.content)
        .collect();
    assert_eq!(users.len(), 2, "the first message and one rework resume");
    // The body comes first, then the section with the named range only.
    let section = "## Read first\n\n### `notes/brief.md:2-3`\n\n```\nline two\nline three\n```";
    let first = &users[0];
    assert!(
        first.starts_with("Create done.txt.\n\n"),
        "body first: {first}"
    );
    assert!(
        first.contains(&format!("\n\n{section}\n\nThe workspace")),
        "{first}"
    );
    assert!(!first.contains("line one"), "{first}");
    // The rework resume carries the failing check output, not the files again.
    let rework = &users[1];
    assert!(
        rework.starts_with("The harness ran the acceptance checks"),
        "{rework}"
    );
    assert!(!rework.contains("Read first"), "{rework}");
    assert!(!rework.contains("line two"), "{rework}");

    // Dispatch features: the assignment length counts the read-first contents.
    let conn = rusqlite::Connection::open(&db).unwrap();
    let tokens: i64 = conn
        .query_row(
            "SELECT assignment_tokens FROM delegation_attempts WHERE rework_index = 0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let encoder = tiktoken_rs::o200k_base_singleton();
    let expected =
        encoder.encode_ordinary("Create done.txt.").len() + encoder.encode_ordinary(section).len();
    assert_eq!(tokens as usize, expected);
}

#[tokio::test]
async fn read_first_errors_reject_the_assignment_before_any_run() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("ws");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(root.path().join("secret.md"), "outside\n").unwrap();
    std::fs::write(workspace.join("short.md"), "only\n").unwrap();
    let db = root.path().join("delegations.db");
    let mut ledger = Ledger::open(&db).unwrap();
    for (entry, expected) in [
        ("missing.md", "`missing.md` is not a file in the workspace"),
        (
            "../secret.md",
            "`../secret.md` resolves outside the workspace",
        ),
        ("short.md:1-4", "`short.md:1-4` ends at line 4"),
    ] {
        // No scripted steps: a model call would not produce this error.
        let f = fixture(&workspace, vec![]);
        let error = delegate::run(
            f.harness.clone(),
            &f.store,
            &mut ledger,
            DelegateRun {
                assignment: read_first_assignment(1, &format!("\"{entry}\"")),
                route: route(),
                workspace: workspace.clone(),
                title: "delegate test".into(),
                strategy: Default::default(),
                events: None,
                collector: Some(f.collector.clone()),
            },
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains(expected), "{error}");
    }
    let report = ledger.report().unwrap();
    assert!(
        report["groups"].as_array().unwrap().is_empty(),
        "no ledger row for a rejected assignment: {report}"
    );
}

/// A model that answers only once its run is cancelled; counts the calls it received.
struct BlockingProvider {
    started: Arc<Notify>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for BlockingProvider {
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "scripted".into(),
            name: "Blocking (test)".into(),
        }]
    }
    async fn generate(
        &self,
        _request: ModelRequest,
        _on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> anyhow::Result<ModelResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        cancellation.cancelled().await;
        anyhow::bail!("cancelled by the test")
    }
}

/// A check that records the pid of a child it started, then waits for it.
#[cfg(windows)]
const SLOW_CHECK: &str =
    "Out-File -FilePath started.txt -InputObject started; Start-Sleep -Seconds 300";
#[cfg(not(windows))]
const SLOW_CHECK: &str = "sleep 300 & echo $! > child.pid; echo started > started.txt; wait";

fn assignment_with_checks(rework: u32, checks: &[&str]) -> Assignment {
    let list: Vec<String> = checks
        .iter()
        .map(|check| toml::Value::String((*check).into()).to_string())
        .collect();
    Assignment::parse(&format!(
        "+++\ncategory = \"implement\"\ntags = [\"test\"]\nchecks = [{}]\nrework = {rework}\nroute = {{ class = \"small\" }}\n+++\nCreate done.txt.\n",
        list.join(", ")
    ))
    .unwrap()
}

fn cancel_request(
    workspace: &Path,
    assignment: Assignment,
    collector: &Arc<Collector>,
) -> DelegateRun {
    DelegateRun {
        assignment,
        route: route(),
        workspace: workspace.to_owned(),
        title: "cancel test".into(),
        strategy: Default::default(),
        events: None,
        collector: Some(collector.clone()),
    }
}

/// `(run_status, final_status, rework_index)` of every ledger row.
fn attempt_rows(db: &Path) -> Vec<(String, String, i64)> {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.prepare(
        "SELECT run_status, final_status, rework_index FROM delegation_attempts ORDER BY rework_index",
    )
    .unwrap()
    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

async fn wait_for_file(path: &Path) {
    for _ in 0..600 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{} never appeared", path.display());
}

/// True once `pid` is gone; a killed child can stay a zombie until init reaps it.
#[cfg(unix)]
async fn wait_until_gone(pid: i32) -> bool {
    for _ in 0..100 {
        let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(") ")
                .is_some_and(|(_, state)| state.starts_with('Z'))
        });
        if zombie || unsafe { libc::kill(pid, 0) } != 0 {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test]
async fn cancelling_a_slow_model_run_records_one_failed_attempt_without_rework() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("ws");
    std::fs::create_dir(&workspace).unwrap();
    let started = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(BlockingProvider {
        started: started.clone(),
        calls: calls.clone(),
    });
    let f = fixture_with_provider(&workspace, provider);
    let db = root.path().join("delegations.db");
    let (mut ledger, _keep) = wal_ledger(&db);
    let before = wal_commits(&db);
    let token = CancellationToken::new();
    let sessions = Arc::new(Mutex::new(Vec::new()));
    let seen = sessions.clone();
    let on_session: Arc<dyn Fn(&str) + Send + Sync> =
        Arc::new(move |id: &str| seen.lock().unwrap().push(id.to_owned()));
    let run = delegate::run_with_control(
        f.harness.clone(),
        &f.store,
        &mut ledger,
        cancel_request(&workspace, assignment(2), &f.collector),
        token.clone(),
        Some(on_session),
    );
    let cancel = async {
        started.notified().await;
        token.cancel();
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(60), async { tokio::join!(run, cancel) })
            .await
            .expect("a cancelled model run ends");
    let result = result.unwrap();
    assert_eq!(result.status, "failed", "{result:#?}");
    assert_eq!(result.attempts, 1, "{result:#?}");
    assert!(
        result
            .error
            .as_deref()
            .unwrap_or_default()
            .to_lowercase()
            .contains("cancelled"),
        "{result:#?}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "no rework after the cancel"
    );
    assert_eq!(*sessions.lock().unwrap(), vec![result.session_id.clone()]);
    assert_eq!(
        wal_commits(&db) - before,
        1,
        "one transaction for the attempt"
    );
    assert_eq!(
        attempt_rows(&db),
        vec![("cancelled".to_owned(), "failed".to_owned(), 0)]
    );
}

#[tokio::test]
async fn cancelling_a_running_check_kills_its_processes_and_records_one_failure() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("ws");
    std::fs::create_dir(&workspace).unwrap();
    let steps = vec![report("done", json!([]), json!([]))];
    let f = fixture(&workspace, steps);
    let db = root.path().join("delegations.db");
    let (mut ledger, _keep) = wal_ledger(&db);
    let before = wal_commits(&db);
    let token = CancellationToken::new();
    let assignment = assignment_with_checks(2, &[SLOW_CHECK, "echo later > later.txt"]);
    let run = delegate::run_with_control(
        f.harness.clone(),
        &f.store,
        &mut ledger,
        cancel_request(&workspace, assignment, &f.collector),
        token.clone(),
        None,
    );
    let cancel = async {
        wait_for_file(&workspace.join("started.txt")).await;
        token.cancel();
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(60), async { tokio::join!(run, cancel) })
            .await
            .expect("a cancelled check ends");
    let result = result.unwrap();
    assert_eq!(result.status, "failed", "{result:#?}");
    assert_eq!(result.attempts, 1, "{result:#?}");
    assert!(result.checks.is_empty(), "{:?}", result.checks);
    assert_eq!(
        result.error.as_deref(),
        Some("cancelled during the acceptance checks")
    );
    assert!(
        !workspace.join("later.txt").exists(),
        "no later check starts after the cancel"
    );
    assert_eq!(
        wal_commits(&db) - before,
        1,
        "one transaction for the attempt"
    );
    assert_eq!(
        attempt_rows(&db),
        vec![("cancelled".to_owned(), "failed".to_owned(), 0)]
    );
    #[cfg(unix)]
    {
        let child: i32 = std::fs::read_to_string(workspace.join("child.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            wait_until_gone(child).await,
            "the check's child process {child} is still running"
        );
    }
}

#[path = "delegate/cancelled_attempt.rs"]
mod cancelled_attempt;
