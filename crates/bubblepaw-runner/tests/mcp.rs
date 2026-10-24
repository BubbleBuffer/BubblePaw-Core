//! MCP through the real runner and a deterministic provider: no live models or servers.
use anyhow::{Result, ensure};
use bubblepaw_core::{
    ApprovalMode, EventKind, Harness, HarnessConfig, Role, RunOptions, Store, ToolCall,
    ToolRegistry,
};
use bubblepaw_mcp::{McpHost, McpSettings};
use bubblepaw_runner::{
    artifacts::ArtifactStore,
    codemode::CodeMode,
    provider::{ScriptedProvider, ScriptedStep},
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::Arc,
    time::Duration,
};

const SECRET: &str = "synthetic-mcp-test-secret";
const TOOL: &str = "mcp__fixture__echo";
const CODE: &str = "const listed = await tools.$list(); if (!listed.some(t => t.name === 'mcp__fixture__echo')) throw new Error('MCP tool missing'); try { const value = await tools.mcp__fixture__echo({text:'MCP accepted'}); text(JSON.stringify(value)); } catch (e) { text(e.message); }";

struct Fixture {
    root: tempfile::TempDir,
    config: PathBuf,
}
impl Fixture {
    fn new() -> Result<Self> {
        let root = tempfile::tempdir()?;
        let config = root.path().join("config.toml");
        let value = json!({
            "model":{"main":"fixture/direct-model"},
            "agent":{"execution_mode":"js"},
            "context":{"auto_summarize":false},
            "storage":{"data_dir":root.path().join("data")},
            "extensions":{"fixture":{"command":env!("CARGO_BIN_EXE_bubblepaw-fake-extension")}},
            "mcp":{"servers":{"fixture":{
                "command":env!("CARGO_BIN_EXE_bubblepaw-runner-fake-mcp"),
                "args":["--log",root.path().join("mcp.jsonl"),"--pid-file",root.path().join("mcp.pid")],
                "env_from":{"MCP_SECRET":"BUBBLEPAW_TEST_MCP_SECRET"},
                "startup_timeout_secs":5,"call_timeout_secs":5
            }}}
        });
        fs::write(&config, toml::to_string(&value)?)?;
        Ok(Self { root, config })
    }
    fn path(&self) -> &Path {
        self.root.path()
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bubblepaw"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("BUBBLEPAW_") {
                command.env_remove(key);
            }
        }
        command
            .arg("--config")
            .arg(&self.config)
            .arg("--workspace")
            .arg(self.path())
            .env("CODEX_HOME", self.path().join("absent-codex"))
            .env("BUBBLEPAW_TEST_MCP_SECRET", SECRET);
        command
    }
    fn requests(&self) -> Result<Vec<Value>> {
        fs::read_to_string(self.path().join("mcp.jsonl"))?
            .lines()
            .map(|line| {
                let entry: Value = serde_json::from_str(line)?;
                Ok(entry["request"].clone())
            })
            .collect()
    }
    fn effects(&self) -> Result<Vec<(String, String)>> {
        let path = fs::read_dir(self.path().join("data/chats"))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .find(|path| path.extension().is_some_and(|extension| extension == "db"))
            .expect("a chat database");
        let connection = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let mut statement = connection.prepare("SELECT state,call_id FROM effects")?;
        Ok(statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }
}

fn output_text(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn config_shows_env_names_and_env_override_without_spawning_servers() -> Result<()> {
    let fixture = Fixture::new()?;
    let output = fixture
        .command()
        .env("BUBBLEPAW_MCP__SERVERS__FIXTURE__CALL_TIMEOUT_SECS", "7")
        .env(
            "BUBBLEPAW_MCP__SERVERS__FIXTURE__ENV_FROM",
            "{ MCP_SECRET = 'BUBBLEPAW_LATE_MCP_SECRET' }",
        )
        .env("BUBBLEPAW_LATE_MCP_SECRET", SECRET)
        .arg("config")
        .output()?;
    ensure!(output.status.success(), "{}", output_text(&output));
    let text = output_text(&output);
    assert!(!text.contains(SECRET), "{text}");
    let config: toml::Value = String::from_utf8(output.stdout)?.parse()?;
    assert_eq!(
        config["mcp"]["servers"]["fixture"]["env_from"]["MCP_SECRET"].as_str(),
        Some("BUBBLEPAW_LATE_MCP_SECRET")
    );
    assert_eq!(
        config["mcp"]["servers"]["fixture"]["call_timeout_secs"].as_integer(),
        Some(7)
    );
    assert!(!fixture.path().join("mcp.pid").exists());
    assert!(!fixture.path().join("mcp.jsonl").exists());
    assert!(!fixture.path().join("data").exists());
    Ok(())
}

#[test]
fn approve_all_runs_mcp_and_records_its_completed_effect() -> Result<()> {
    let fixture = Fixture::new()?;
    let output = fixture
        .command()
        .args(["--approve-all", "--json", "run", &format!("js {CODE}")])
        .output()?;
    let text = output_text(&output);
    ensure!(output.status.success(), "{text}");
    assert!(text.contains("MCP accepted"), "{text}");
    assert!(!text.contains(SECRET), "{text}");
    let requests = fixture.requests()?;
    assert_eq!(
        requests
            .iter()
            .filter(|request| request["method"] == "tools/call")
            .count(),
        1
    );
    let effects = fixture.effects()?;
    assert_eq!(effects.len(), 2, "outer js and nested MCP: {effects:?}");
    assert!(effects.iter().all(|(state, _)| state == "completed"));
    let events = String::from_utf8(output.stdout)?;
    let call_id = events
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<serde_json::Result<Vec<_>>>()?
        .into_iter()
        .find(|event| event["type"] == "tool_started" && event["call"]["name"] == TOOL)
        .expect("MCP call started")["call"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(effects.iter().any(|(_, id)| id == &call_id));
    #[cfg(unix)]
    {
        let pid: i32 = fs::read_to_string(fixture.path().join("mcp.pid"))?
            .trim()
            .parse()?;
        // SAFETY: signal 0 checks whether the fixture process still exists.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    Ok(())
}

#[test]
fn one_shot_run_denies_mcp_without_sending_a_call_or_recording_its_effect() -> Result<()> {
    let fixture = Fixture::new()?;
    let output = fixture
        .command()
        .args(["--json", "run", &format!("js {CODE}")])
        .output()?;
    let text = output_text(&output);
    ensure!(output.status.success(), "{text}");
    assert!(
        text.contains("no client to approve; use --approve-all"),
        "{text}"
    );
    assert!(text.contains("\"state\":\"denied\""), "{text}");
    assert!(!text.contains(SECRET), "{text}");
    assert!(
        fixture
            .requests()?
            .iter()
            .all(|request| request["method"] != "tools/call")
    );
    let effects = fixture.effects()?;
    assert_eq!(effects.len(), 1, "only the outer js: {effects:?}");
    let events = String::from_utf8(output.stdout)?;
    assert!(
        events
            .lines()
            .map(serde_json::from_str::<Value>)
            .all(|event| {
                let event = event.unwrap();
                event["type"] != "tool_started" || event["call"]["name"] != TOOL
            })
    );
    Ok(())
}

#[tokio::test]
async fn scripted_provider_calls_mcp_through_js_and_keeps_the_effect_boundary() -> Result<()> {
    let fixture = Fixture::new()?;
    let config: toml::Value = fs::read_to_string(&fixture.config)?.parse()?;
    let mut mcp = config["mcp"].clone();
    mcp["servers"]["fixture"]
        .as_table_mut()
        .unwrap()
        .remove("env_from");
    let settings: McpSettings = mcp.try_into()?;
    let host = McpHost::start(&settings, fixture.path()).await;
    ensure!(host.warnings().is_empty(), "{:?}", host.warnings());
    let mut tools = ToolRegistry::new();
    for tool in host.tools() {
        tools.register_host(tool)?;
    }
    let repl = Arc::new(CodeMode::new(
        fixture.path().to_owned(),
        Arc::new(ArtifactStore::new(
            fixture.path(),
            fixture.path().join("artifacts"),
        )?),
    )?);
    repl.register(&mut tools)?;
    assert!(tools.definitions()[0].description.contains(TOOL));
    let provider = Arc::new(ScriptedProvider::with_steps(
        vec![ScriptedStep::Calls {
            calls: vec![ToolCall {
                id: "scripted-mcp".into(),
                name: "js".into(),
                arguments: json!({"code":CODE}),
            }],
        }],
        "finished",
    ));
    let store = Store::open_data_dir(fixture.path().join("scripted-data"))?;
    let session = store.create_session("MCP scripted acceptance")?;
    let mut config = HarnessConfig {
        execution_mode: bubblepaw_core::ExecutionMode::Js,
        ..Default::default()
    };
    config.approval.confirm = ApprovalMode::Allow;
    let harness = Harness::new(store.clone(), provider, tools, config);
    let mut events = harness.subscribe();
    let run = harness
        .prompt(&session.id, "call MCP", RunOptions::default())
        .await?;
    ensure!(run.status == "completed", "{run:?}");
    let results = store.messages(&session.id)?;
    assert!(
        results
            .iter()
            .any(|message| message.role == Role::Tool && message.content.contains("MCP accepted"))
    );
    assert!(store.pending_effects(&session.id)?.is_empty());
    let mut mcp_call_id = None;
    while let Ok(event) = events.try_recv() {
        if let EventKind::ToolStarted { call } = event.kind
            && call.name == TOOL
        {
            mcp_call_id = Some(call.id);
        }
    }
    let mcp_call_id = mcp_call_id.expect("MCP call started");
    let connection = rusqlite::Connection::open(store.chat_path(&session.id)?.unwrap())?;
    let count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM effects WHERE state='completed' AND call_id=?1",
        [mcp_call_id],
        |row| row.get(0),
    )?;
    assert_eq!(count, 1);
    harness.shutdown(Duration::from_secs(2)).await?;
    repl.shutdown().await;
    host.shutdown(Duration::from_secs(2)).await;
    Ok(())
}
