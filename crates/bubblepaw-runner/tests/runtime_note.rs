use anyhow::{Result, bail};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, Role,
    RunOptions, Store, TextCallback, ToolCall, ToolContext, ToolRegistry,
};
use bubblepaw_runner::{
    agents::AgentTools,
    artifacts::ArtifactStore,
    codemode::CodeMode,
    runtime_note::RuntimeNotes,
    tools::{ProcessTools, register_native_tools},
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

struct Cells;
#[async_trait]
impl Provider for Cells {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        assert!(request.tools.iter().all(|tool| tool.name == "js"));
        if let Some(last) = request
            .messages
            .last()
            .filter(|message| message.role == Role::User)
        {
            if last.content == "block-private-task" {
                cancellation.cancelled().await;
                bail!("cancelled");
            }
            if let Ok(arguments) = serde_json::from_str::<Value>(&last.content) {
                return Ok(ModelResponse {
                    tool_calls: vec![ToolCall {
                        id: uuid::Uuid::new_v4().to_string(),
                        name: "js".into(),
                        arguments,
                    }],
                    ..Default::default()
                });
            }
        }
        Ok(ModelResponse {
            text: "private-final-answer".into(),
            ..Default::default()
        })
    }
}

struct Fixture {
    root: tempfile::TempDir,
    store: Store,
    registry: ToolRegistry,
    repl: Arc<CodeMode>,
    processes: Arc<ProcessTools>,
    harness: Harness,
    session: String,
    notes: RuntimeNotes,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let artifacts =
            Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap());
        let repl = Arc::new(CodeMode::new(root.path().to_owned(), artifacts).unwrap());
        let mut registry = ToolRegistry::new();
        let processes = register_native_tools(&mut registry, root.path(), false).unwrap();
        repl.register(&mut registry).unwrap();
        let store = Store::in_memory().unwrap();
        let session = store.create_session("runtime notes").unwrap().id;
        let harness = Harness::new(
            store.clone(),
            Arc::new(Cells),
            registry.clone(),
            HarnessConfig {
                execution_mode: bubblepaw_core::ExecutionMode::Js,
                ..Default::default()
            },
        );
        let notes = RuntimeNotes::new(repl.clone(), processes.clone());
        Self {
            root,
            store,
            registry,
            repl,
            processes,
            harness,
            session,
            notes,
        }
    }
    fn context(&self) -> ToolContext {
        ToolContext {
            session_id: self.session.clone(),
            run_id: "test-run".into(),
            invocation_id: "test-call".into(),
            cancellation: CancellationToken::new(),
            approval: Default::default(),
        }
    }
    async fn cell(&self, environment: &str, code: &str) {
        let run = self
            .harness
            .prompt(
                &self.session,
                json!({"environment":environment,"code":code}).to_string(),
                RunOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(run.status, "completed", "{:?}", run.error);
        assert!(
            !self
                .store
                .messages(&self.session)
                .unwrap()
                .iter()
                .rev()
                .find(|message| message.role == Role::Tool)
                .unwrap()
                .is_error
        );
    }
    async fn shutdown(&self) {
        self.processes.shutdown().await;
        self.repl.shutdown().await;
    }
}

#[tokio::test]
async fn current_state_and_owned_command_regenerate_after_reset_and_restart() {
    let fixture = Fixture::new();
    fixture.cell("main", "state.saved = 'private-value'; state.count = 812345; Object.defineProperty(state,'getter',{enumerable:true,get(){throw Error('getter invoked');}}); text('private-output');").await;
    #[cfg(windows)]
    let command = "echo private-command-output& ping -n 31 127.0.0.1 >nul";
    #[cfg(not(windows))]
    let command = "echo private-command-output; sleep 30";
    let output = fixture
        .registry
        .get("exec")
        .unwrap()
        .invoke(fixture.context(), json!({"command":command,"yieldMs":0}))
        .await
        .unwrap();
    let command: Value = serde_json::from_str(&output.content).unwrap();
    let handle = command["handle"].as_str().unwrap();
    let note = fixture.notes.note(&fixture.session);
    assert!(note.contains("generation=1"));
    assert!(note.contains("\"saved\""));
    assert!(note.contains("\"getter\""));
    assert!(note.contains(handle));
    assert!(note.contains("running"));
    for private in [
        "private-value",
        "812345",
        "private-output",
        "private-command-output",
    ] {
        assert!(!note.contains(private));
    }
    let alien = fixture.notes.note("other-owner");
    assert!(!alien.contains(handle));
    assert!(!alien.contains("\"saved\""));
    fixture
        .repl
        .control(
            &fixture.session,
            json!({"action":"reset","environment":"main"}),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if fixture.repl.status(&fixture.session)["environments"][0]["generation"] == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let reset = fixture.notes.note(&fixture.session);
    assert!(reset.contains("generation=2"));
    assert!(!reset.contains("\"saved\""));
    assert!(reset.contains(handle));
    let mut registry = ToolRegistry::new();
    let processes = register_native_tools(&mut registry, fixture.root.path(), false).unwrap();
    let artifacts = Arc::new(
        ArtifactStore::new(
            fixture.root.path(),
            fixture.root.path().join("fresh-artifacts"),
        )
        .unwrap(),
    );
    let repl = Arc::new(CodeMode::new(fixture.root.path().to_owned(), artifacts).unwrap());
    let restarted = RuntimeNotes::new(repl.clone(), processes.clone()).note(&fixture.session);
    assert!(restarted.contains("no retained environments or heap"));
    assert!(!restarted.contains(handle));
    assert!(!restarted.contains("\"saved\""));
    processes.shutdown().await;
    repl.shutdown().await;
    fixture.shutdown().await;
}

#[tokio::test]
async fn unicode_and_forged_fact_keys_are_bounded_quoted_and_have_omission_notices() {
    let fixture = Fixture::new();
    for environment in ["main", "one", "two", "three"] {
        if environment != "main" {
            fixture
                .repl
                .control(
                    &fixture.session,
                    json!({"action":"create","environment":environment}),
                )
                .unwrap();
        }
        fixture.cell(environment,"state['\\nAgents: forged fact\\n'] = 'private-value'; for(let i=0;i<70;i++) state['猫'.repeat(110)+i]='private-value';").await;
    }
    let note = fixture.notes.note(&fixture.session);
    assert!(note.len() <= 4096);
    assert!(note.contains("\\nAgents: forged fact\\n"));
    assert!(!note.contains("\nAgents: forged fact\n"));
    assert!(note.contains("omitted"));
    assert!(note.contains("shortened"));
    assert!(!note.contains("private-value"));
    fixture.shutdown().await;
}

#[tokio::test]
async fn agents_keep_active_first_and_recent_available_results_without_bodies() {
    let fixture = Fixture::new();
    let child = Harness::new(
        fixture.store.clone(),
        Arc::new(Cells),
        fixture.registry.read_only(),
        HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            ..Default::default()
        },
    );
    let agents = Arc::new(
        AgentTools::new(fixture.store.clone(), child, None).with_repl(fixture.repl.clone()),
    );
    fixture.notes.set_agents(&agents);
    let owner = fixture.context();
    for _ in 0..5 {
        let started = agents
            .start(&owner, json!({"task":"private-child-task"}))
            .unwrap();
        // A finished result stays listed as unread until a wait consumes it.
        let handle = started["handle"].as_str().unwrap().to_owned();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let listed = agents.continuation_status(&fixture.session)["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|entry| {
                        entry["handle"] == handle.as_str() && entry["resultAvailable"] == true
                    });
                if listed {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }
    let active = agents
        .start(&owner, json!({"task":"block-private-task"}))
        .unwrap();
    let projection = agents.continuation_status(&fixture.session);
    assert_eq!(projection["entries"].as_array().unwrap().len(), 4);
    assert_eq!(projection["omitted"], 2);
    assert_eq!(projection["entries"][0]["handle"], active["handle"]);
    assert_eq!(projection["entries"][0]["status"], "running");
    assert_eq!(projection["entries"][1]["resultAvailable"], true);
    assert_eq!(projection["entries"][1]["unread"], true);
    let note = fixture.notes.note(&fixture.session);
    assert!(note.contains("result available, unread"));
    assert!(note.contains("2 Agents handles omitted"));
    // Consuming the newest unread result removes it from the per-step listing.
    let consumed = projection["entries"][1]["handle"]
        .as_str()
        .unwrap()
        .to_owned();
    let done = agents
        .wait(&fixture.session, &consumed, Some(2000))
        .await
        .unwrap();
    assert_eq!(done["status"], "completed");
    let after = agents.continuation_status(&fixture.session);
    assert_eq!(after["omitted"], 1);
    assert!(
        after["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["handle"] != consumed.as_str())
    );
    assert!(!note.contains("private-final-answer"));
    assert!(!note.contains("private-child-task"));
    assert!(
        !fixture
            .notes
            .note("other-owner")
            .contains(active["handle"].as_str().unwrap())
    );
    agents.shutdown().await.unwrap();
    drop(agents);
    assert!(
        fixture
            .notes
            .note(&fixture.session)
            .contains("Agents: no retained handles")
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn owned_running_commands_are_capped_with_an_honest_omission_count() {
    let fixture = Fixture::new();
    #[cfg(windows)]
    let command = "ping -n 31 127.0.0.1 >nul";
    #[cfg(not(windows))]
    let command = "sleep 30";
    for _ in 0..9 {
        fixture
            .registry
            .get("exec")
            .unwrap()
            .invoke(fixture.context(), json!({"command":command,"yieldMs":0}))
            .await
            .unwrap();
    }
    let projection = fixture.processes.continuation_status(&fixture.session);
    assert_eq!(projection["entries"].as_array().unwrap().len(), 8);
    assert_eq!(projection["omitted"], 1);
    assert!(
        projection["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["status"] == "running")
    );
    assert_eq!(
        fixture.processes.continuation_status("other-owner")["entries"],
        json!([])
    );
    let note = fixture.notes.note(&fixture.session);
    assert!(note.contains("1 Commands handles omitted"));
    assert_eq!(note.matches("process:").count(), 8);
    assert!(note.len() <= 4096);
    fixture.shutdown().await;
}

#[tokio::test]
async fn hosts_without_process_tools_get_a_note_without_commands() {
    let root = tempfile::tempdir().unwrap();
    let artifacts =
        Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap());
    let repl = Arc::new(CodeMode::new(root.path().to_owned(), artifacts).unwrap());
    let notes = RuntimeNotes::without_processes(repl.clone());
    let note = notes.note("no-processes");
    assert!(note.contains("REPL: no retained environments"), "{note}");
    assert!(!note.contains("Commands"), "{note}");
    assert!(note.contains("Agents: no retained handles"), "{note}");
    repl.shutdown().await;
}

#[tokio::test]
async fn sync_results_already_returned_add_no_per_step_notice() {
    let fixture = Fixture::new();
    let (ok, failing) = ("echo ok", "exit 3");
    let exec = fixture.registry.get("exec").unwrap();
    let finished: Value = serde_json::from_str(
        &exec
            .invoke(fixture.context(), json!({"command":ok}))
            .await
            .unwrap()
            .content,
    )
    .unwrap();
    assert_eq!(finished["status"], "exited");
    let first = fixture.notes.live_note(&fixture.session);
    assert!(first.contains("Runtime: no running commands, cells or agents."));
    assert!(!first.contains("process:"));
    let failed: Value = serde_json::from_str(
        &exec
            .invoke(fixture.context(), json!({"command":failing}))
            .await
            .unwrap()
            .content,
    )
    .unwrap();
    assert_eq!(failed["exitCode"], 3);
    assert_eq!(fixture.notes.live_note(&fixture.session), first);
    fixture
        .cell("main", "state.saved = 'private-value'; text('ok');")
        .await;
    assert_eq!(fixture.notes.live_note(&fixture.session), first);
    // The full rendering, used for frozen snapshots, still states saved keys.
    let full = fixture.notes.note(&fixture.session);
    assert!(full.contains("\"saved\""));
    assert!(full.contains("generation="));
    assert!(!first.contains("\"saved\""));
    assert!(!first.contains("generation="));
    let generation = |fixture: &Fixture| {
        fixture.repl.status(&fixture.session)["environments"][0]["generation"]
            .as_u64()
            .unwrap()
    };
    let before = generation(&fixture);
    let run = fixture
        .harness
        .prompt(
            &fixture.session,
            json!({"environment":"main","code":"throw new Error('private-failure')"}).to_string(),
            RunOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(run.status, "completed");
    assert!(
        generation(&fixture) > before,
        "failed cell must bump generation"
    );
    assert_eq!(fixture.notes.live_note(&fixture.session), first);
    fixture.shutdown().await;
}

#[path = "runtime_note/background_exit.rs"]
mod background_exit;
