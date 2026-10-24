//! Scripted host acceptance for isolated children through the production
//! LiveConfig, child_agent_host_live and worktree adapters. No model or
//! external service is contacted.
use anyhow::{Result, ensure};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, HarnessConfig, ModelRequest, ModelResponse, Provider, Role, Store,
    TextCallback, ToolCall, ToolContext, ToolRegistry,
};
use bubblepaw_mcp::McpHost;
use bubblepaw_runner::{
    agent_handles::register_agent_handles,
    agents::{AgentRegistry, AgentTools},
    artifacts::ArtifactStore,
    codemode::CodeMode,
    config::{EffectiveConfig, LoadOptions},
    live_config::LiveConfig,
    profiles::child_agent_host_live,
    provider::LiveProviderConfig,
    register_native_tools_with_exec,
    tools::ProcessTools,
    worktrees,
};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

#[path = "support/eventually.rs"]
mod eventually;
use eventually::eventually;

/// Each session's first js task becomes one tool call; later turns finish with text.
#[derive(Default)]
struct Script {
    tasks: Mutex<HashSet<(String, String)>>,
    requests: Mutex<Vec<ModelRequest>>,
}
#[async_trait]
impl Provider for Script {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let task = request
            .messages
            .iter()
            .rev()
            .filter(|message| message.role == Role::User)
            .find_map(|message| {
                message
                    .content
                    .lines()
                    .next()
                    .and_then(|line| serde_json::from_str::<Value>(line).ok())
            })
            .unwrap_or(json!({}));
        let first = self
            .tasks
            .lock()
            .unwrap()
            .insert((request.session_id.clone(), task.to_string()));
        self.requests.lock().unwrap().push(request);
        Ok(match task["code"].as_str().filter(|_| first) {
            Some(code) => ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "js".into(),
                    arguments: json!({"code": code}),
                }],
                ..Default::default()
            },
            None => ModelResponse {
                text: "script finished".into(),
                ..Default::default()
            },
        })
    }
}

struct Host {
    store: Store,
    script: Arc<Script>,
    live: Arc<LiveConfig>,
    agents: Arc<AgentTools>,
    repl: Arc<CodeMode>,
    processes: Arc<ProcessTools>,
    mcp: Arc<McpHost>,
}
impl Host {
    /// Production wiring: LiveConfig owns the tool generation, and
    /// child_agent_host_live installs the isolated workspace host.
    async fn start(directory: &Path, repo: &Path, text: &str) -> Result<Self> {
        let file = directory.join("config").join("config.toml");
        write_file(&file, text)?;
        let effective = EffectiveConfig::load(LoadOptions {
            workspace: repo.to_owned(),
            home: None,
            custom_file: Some(file),
            env: vec![],
            cli: vec![],
        })?;
        let settings = effective.settings()?;
        let data = directory.join("data");
        let store = Store::open_data_dir(data.clone())?;
        let mut tools = ToolRegistry::new();
        let processes = register_native_tools_with_exec(
            &mut tools,
            repo,
            false,
            settings.exec.clone(),
            data.clone(),
        )?;
        let mcp = Arc::new(McpHost::start(&settings.mcp, repo).await);
        let providers = Arc::new(LiveProviderConfig::new(effective.clone())?);
        let live = LiveConfig::new(
            effective,
            providers.clone(),
            mcp.clone(),
            processes.clone(),
            store.clone(),
        )?;
        let repl = Arc::new(CodeMode::new(
            repo.to_owned(),
            Arc::new(ArtifactStore::new(repo, data.join("artifacts"))?),
        )?);
        let script = Arc::new(Script::default());
        let mut harness = HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            model: "demo/first".into(),
            ..Default::default()
        };
        live.configure_harness(&mut harness);
        let agents = child_agent_host_live(
            store.clone(),
            script.clone(),
            &tools,
            &repl,
            &harness,
            repo,
            true,
            None,
            settings.agents,
            Arc::new(AgentRegistry::default()),
            processes.clone(),
            providers,
        )?;
        agents.register(&mut tools)?;
        register_agent_handles(&mut tools, agents.clone())?;
        repl.register(&mut tools)?;
        live.bind(tools, &agents)?;
        Ok(Self {
            store,
            script,
            live,
            agents,
            repl,
            processes,
            mcp,
        })
    }
    async fn stop(&self) -> Result<()> {
        self.agents.shutdown().await?;
        self.repl.shutdown().await;
        self.processes.shutdown().await;
        self.mcp.shutdown(Duration::from_secs(2)).await;
        Ok(())
    }
    fn spawn(&self, owner: &str, code: &str) -> Result<String> {
        let child = self.agents.start(
            &context(owner),
            json!({"task": task(code), "profile": "workspace", "worktree": true}),
        )?;
        Ok(child["sessionId"].as_str().unwrap().to_owned())
    }
    fn send(&self, from: &str, child: &str, code: &str) -> Result<()> {
        self.agents.send(
            &context(from),
            json!({"sessionId": child, "text": task(code)}),
        )?;
        Ok(())
    }
    async fn finish(&self, owner: &str, child: &str) -> Result<()> {
        let result = self.agents.wait(owner, child, Some(30_000)).await?;
        ensure!(
            result["status"] == "completed",
            "child did not complete: {result}"
        );
        let errors: Vec<String> = self
            .store
            .messages(child)?
            .into_iter()
            .filter(|message| message.is_error)
            .map(|message| message.content)
            .collect();
        ensure!(errors.is_empty(), "child tool failed: {errors:?}");
        Ok(())
    }
    /// Structured js return values in order. A large return is its receipt.
    fn results(&self, session: &str) -> Result<Vec<Value>> {
        let mut values: Vec<Value> = Vec::new();
        for message in self.store.messages(session)? {
            if message.role != Role::Tool {
                continue;
            }
            ensure!(!message.is_error, "{}", message.content);
            let text = message.content.split("\n[---]\n").next().unwrap();
            values.push(serde_json::from_str(text)?);
        }
        Ok(values)
    }
    /// The system prompt the session saw on its first model request.
    fn first_prompt(&self, session: &str) -> String {
        self.script
            .requests
            .lock()
            .unwrap()
            .iter()
            .find(|request| request.session_id == session)
            .map(|request| request.system_prompt.clone())
            .unwrap()
    }
}

fn config(extra: &str) -> String {
    format!(
        "[model]\nmain='demo/first'\n[approvals]\nconfirm='allow'\n[roles]\nsummary='main'\nchapter='main'\ndecider='main'\napprover='main'\n{extra}"
    )
}
fn context(session: &str) -> ToolContext {
    ToolContext {
        session_id: session.into(),
        run_id: String::new(),
        invocation_id: uuid::Uuid::new_v4().to_string(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    }
}
fn task(code: &str) -> String {
    json!({"code": code}).to_string()
}
fn write_file(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text)?;
    Ok(())
}
fn skill(name: &str, description: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n")
}
fn toml_string(path: &Path) -> String {
    toml::Value::String(path.to_string_lossy().into_owned()).to_string()
}
fn git(path: &Path, arguments: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(arguments)
        .output()?;
    ensure!(
        output.status.success(),
        "git {}: {}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
fn commit(path: &Path, paths: &[&str], message: &str) -> Result<()> {
    let mut add = vec!["add"];
    add.extend_from_slice(paths);
    git(path, &add)?;
    git(
        path,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            message,
        ],
    )?;
    Ok(())
}
/// Committed parent state: one plain file and one project skill.
fn initialize(repo: &Path) -> Result<()> {
    std::fs::create_dir_all(repo)?;
    git(repo, &["init", "-q"])?;
    write_file(&repo.join("base.txt"), "committed parent evidence\n")?;
    write_file(
        &repo.join(".agents/skills/committed-skill/SKILL.md"),
        &skill(
            "committed-skill",
            "COMMITTED-SKILL-DESCRIPTION",
            "COMMITTED-BODY",
        ),
    )?;
    commit(repo, &["."], "initial")
}
fn spill_dir(data: &Path, session: &str) -> PathBuf {
    data.canonicalize().unwrap().join("spill").join(session)
}
/// Same rolling checksum as the js readback, so equal values prove equal bytes.
fn hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0, |hash, byte| {
        (hash * 31 + u64::from(*byte)) % 2_147_483_647
    })
}
/// Writes a marker into the working directory, then prints 20,000 lines. The
/// full result crosses the inline limit and must spill.
#[cfg(windows)]
fn spill_command(marker: &str) -> String {
    format!(
        "[IO.File]::WriteAllText('{marker}','{marker}'); 0..19999 | ForEach-Object {{ 'line-{{0:D5}} payload' -f $_ }}"
    )
}
#[cfg(not(windows))]
fn spill_command(marker: &str) -> String {
    format!(
        "printf '%s' '{marker}' > {marker}; awk 'BEGIN {{ for(i=0;i<20000;i++) printf \"line-%05d payload\\n\", i }}'"
    )
}
#[cfg(windows)]
fn long_command(marker: &str) -> String {
    format!("[IO.File]::WriteAllText('{marker}','{marker}'); Start-Sleep -Seconds 30")
}
#[cfg(not(windows))]
fn long_command(marker: &str) -> String {
    format!("printf '%s' '{marker}' > {marker}; sleep 30")
}
#[cfg(windows)]
fn quick_command() -> &'static str {
    "Write-Output ok"
}
#[cfg(not(windows))]
fn quick_command() -> &'static str {
    "true"
}

const SPILL_CODE: &str = "const rows=Array.from({length:6000},(_,i)=>'row-'+String(i).padStart(5,'0')+' '+'j'.repeat(40)); return {rows};";

/// Reads the external skill body and helper, runs a large exec, then pages its full
/// spill back through tools.read and returns a checksum of exactly what was read.
fn work_code(tag: &str, body_literal: &str, helper_literal: &str) -> String {
    let command = json!(spill_command(&format!("cwd-{tag}.txt")));
    format!(
        "const body=await tools.read({{path:{body_literal}}}); \
const helper=await tools.read({{path:{helper_literal}}}); \
const run=await tools.exec({{command:{command}}}); \
if(run.exitCode!==0||!run.fullOutput) throw Error(JSON.stringify(run)); \
const path=run.fullOutput.path; let full='', start=1, pages=0; \
for(;;) {{ const page=await tools.read({{path,startLine:start,lineCount:1000}}); full+=page.content; pages++; \
if(page.returnedLines===0||start+page.returnedLines>page.totalLines) break; start+=page.returnedLines; }} \
let hash=0; for(let i=0;i<full.length;i++) hash=(hash*31+full.charCodeAt(i))%2147483647; \
return {{tag:'{tag}',external:{{body:body.content.includes('EXTERNAL-BODY-TOKEN'),helper:helper.content.includes('EXTERNAL-HELPER-TOKEN')}},exec:{{path,bytes:full.length,pages,hash}}}};"
    )
}
fn prefix_code(path: &Path) -> String {
    format!(
        "const page=await tools.read({{path:{}, startLine:1, lineCount:1}}); return {{prefix:page.content.startsWith('{{\"rows\":[\"row-00000 '), totalLines:page.totalLines}};",
        json!(path)
    )
}
/// Host tool errors reach JS as thrown Errors (codemode bridge: is_error -> reject), so
/// denial is observed by catching them. A success is reported as ok:true and fails the test.
const OUTCOME: &str = "const outcome=async f=>{try{return {ok:true,value:await f()};}catch(e){return {ok:false,error:String(e)};}};";
fn probe_code(path: &Path) -> String {
    format!(
        "{} const read=await outcome(()=>tools.read({{path:{}}})); return {{read}};",
        OUTCOME,
        json!(path)
    )
}

/// Checks one work cell: external reads, full spill readback, and cwd isolation.
fn check_work(value: &Value, tag: &str, data: &Path, session: &str, worktree: &Path) -> Result<()> {
    let marker = format!("cwd-{tag}.txt");
    assert_eq!(value["tag"], tag);
    assert_eq!(value["external"], json!({"body": true, "helper": true}));
    let path = PathBuf::from(value["exec"]["path"].as_str().unwrap());
    assert!(
        path.starts_with(spill_dir(data, session)),
        "{}",
        path.display()
    );
    let full = std::fs::read(&path)?;
    assert_eq!(value["exec"]["bytes"], full.len());
    assert_eq!(value["exec"]["hash"], hash(&full));
    assert!(value["exec"]["pages"].as_u64().unwrap() > 1);
    // Framing is format-agnostic: every payload line must survive the spill exactly once, in order.
    let text = String::from_utf8(full)?.replace("\r\n", "\n");
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with("line-"))
        .collect();
    assert_eq!(lines.len(), 20_000);
    for (index, line) in lines.iter().enumerate() {
        assert_eq!(*line, format!("line-{index:05} payload"));
    }
    assert_eq!(std::fs::read_to_string(worktree.join(&marker))?, marker);
    Ok(())
}

#[tokio::test]
async fn isolated_children_keep_skills_external_resources_and_owned_spills() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let repo = directory.path().join("repo");
    let data = directory.path().join("data");
    initialize(&repo)?;
    let head = git(&repo, &["rev-parse", "HEAD"])?;
    // Uncommitted parent skill edits must stay out of every isolated child catalog.
    write_file(
        &repo.join(".agents/skills/committed-skill/SKILL.md"),
        &skill(
            "committed-skill",
            "DIRTY-REPLACEMENT-DESCRIPTION",
            "DIRTY-BODY",
        ),
    )?;
    write_file(
        &repo.join(".agents/skills/dirty-only/SKILL.md"),
        &skill("dirty-only", "DIRTY-ONLY-DESCRIPTION", "DIRTY-ONLY-BODY"),
    )?;
    let externals = directory.path().join("external");
    let external = externals.join("external-skill");
    write_file(
        &external.join("SKILL.md"),
        &skill(
            "external-skill",
            "EXTERNAL-DESCRIPTION",
            "EXTERNAL-BODY-TOKEN",
        ),
    )?;
    write_file(&external.join("helper.txt"), "EXTERNAL-HELPER-TOKEN\n")?;
    let host = Host::start(
        directory.path(),
        &repo,
        &config(&format!(
            "[agents]\nauto_wake=false\n[skills]\ndirs=[{}]\n",
            toml_string(&externals)
        )),
    )
    .await?;
    assert!(host.live.registry().get("exec").is_some());
    let owner = host.store.create_session("parent")?.id;
    let body = json!(external.join("SKILL.md")).to_string();
    let helper = json!(external.join("helper.txt")).to_string();

    // Child: committed catalog, external reads, exec spill and JS spill readback.
    let child = host.spawn(&owner, &work_code("child", &body, &helper))?;
    host.finish(&owner, &child).await?;
    let child_path = worktrees::get(&host.store, &child)?.unwrap().path;
    let prompt = host.first_prompt(&child);
    assert!(
        prompt.contains("COMMITTED-SKILL-DESCRIPTION") && prompt.contains("EXTERNAL-DESCRIPTION")
    );
    assert!(
        !prompt.contains("DIRTY-REPLACEMENT-DESCRIPTION")
            && !prompt.contains("DIRTY-ONLY-DESCRIPTION")
    );
    check_work(
        &host.results(&child)?[0],
        "child",
        &data,
        &child,
        &child_path,
    )?;
    assert!(!repo.join("cwd-child.txt").exists());

    host.send(&owner, &child, SPILL_CODE)?;
    host.finish(&owner, &child).await?;
    let receipt = host.results(&child)?[1].clone();
    assert_eq!(receipt["truncated"], true);
    let spill = PathBuf::from(receipt["fullOutput"]["path"].as_str().unwrap());
    assert!(spill.starts_with(spill_dir(&data, &child)));
    let spilled = std::fs::read(&spill)?;
    assert_eq!(receipt["fullOutput"]["bytes"], spilled.len());
    let rows = (0..6000)
        .map(|index| format!("row-{index:05} {}", "j".repeat(40)))
        .collect::<Vec<_>>();
    let text = String::from_utf8(spilled)?;
    assert_eq!(
        serde_json::from_str::<Value>(text.split("\n[---]\n").next().unwrap())?,
        json!({"rows": rows})
    );

    // The owner can read its own JS spill back, starting from the spill path.
    host.send(&owner, &child, &prefix_code(&spill))?;
    host.finish(&owner, &child).await?;
    let readback = host.results(&child)?[2].clone();
    assert_eq!(readback["prefix"], true);
    assert_eq!(readback["totalLines"], 1);

    // Commit a new skill in the child. Its own catalog stays frozen; a grandchild sees it.
    write_file(
        &child_path.join(".agents/skills/child-added/SKILL.md"),
        &skill("child-added", "CHILD-ADDED-DESCRIPTION", "CHILD-ADDED-BODY"),
    )?;
    commit(&child_path, &[".agents"], "child skill")?;
    let child_head = git(&child_path, &["rev-parse", "HEAD"])?;
    assert_ne!(child_head, head);
    assert!(
        !host
            .first_prompt(&child)
            .contains("CHILD-ADDED-DESCRIPTION")
    );

    let grandchild_code = work_code("grandchild", &body, &helper);
    host.send(
        &owner,
        &child,
        &format!(
            "const spawned=await tools.agent({{task:{},profile:'workspace',worktree:true}}); return {{spawned:true}};",
            json!(task(&grandchild_code))
        ),
    )?;
    host.finish(&owner, &child).await?;
    let grandchild = host
        .store
        .list_sessions()?
        .into_iter()
        .find(|session| session.parent_session_id.as_deref() == Some(child.as_str()))
        .unwrap()
        .id;
    host.finish(&child, &grandchild).await?;
    let grandchild_info = worktrees::get(&host.store, &grandchild)?.unwrap();
    assert_eq!(grandchild_info.parent_workspace, child_path);
    assert_eq!(
        git(&grandchild_info.path, &["rev-parse", "HEAD"])?,
        child_head
    );
    let prompt = host.first_prompt(&grandchild);
    assert!(
        prompt.contains("CHILD-ADDED-DESCRIPTION")
            && prompt.contains("COMMITTED-SKILL-DESCRIPTION")
            && prompt.contains("EXTERNAL-DESCRIPTION")
    );
    assert!(
        !prompt.contains("DIRTY-REPLACEMENT-DESCRIPTION")
            && !prompt.contains("DIRTY-ONLY-DESCRIPTION")
    );
    check_work(
        &host.results(&grandchild)?[0],
        "grandchild",
        &data,
        &grandchild,
        &grandchild_info.path,
    )?;
    assert!(!child_path.join("cwd-grandchild.txt").exists());

    // A sibling child cannot read the first child's exec spill.
    let child_spill = PathBuf::from(host.results(&child)?[0]["exec"]["path"].as_str().unwrap());
    let sibling = host.spawn(&owner, &probe_code(&child_spill))?;
    host.finish(&owner, &sibling).await?;
    let probe = host.results(&sibling)?[0].clone();
    let read = &probe["read"];
    assert_eq!(read["ok"], false, "sibling read was not denied: {probe}");
    assert!(
        read["error"]
            .as_str()
            .is_some_and(|error| error.contains("outside this chat")),
        "{probe}"
    );

    // Parent checkout: no child command output, no child skill, and dirty edits intact.
    assert!(!repo.join("cwd-child.txt").exists() && !repo.join("cwd-grandchild.txt").exists());
    assert!(!repo.join(".agents/skills/child-added").exists());
    assert!(
        std::fs::read_to_string(repo.join(".agents/skills/committed-skill/SKILL.md"))?
            .contains("DIRTY-REPLACEMENT-DESCRIPTION")
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD"])?, head);
    host.stop().await?;
    Ok(())
}

#[path = "child_context/process_lifetime.rs"]
mod process_lifetime;
