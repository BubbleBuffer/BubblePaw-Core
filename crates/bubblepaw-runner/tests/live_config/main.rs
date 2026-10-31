//! Scripted host acceptance: no model or external service requests.

use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, MessageOrigin, ModelRequest, ModelResponse,
    Provider, Role, RunOptions, Store, TextCallback, ToolCall, ToolRegistry,
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
};
use serde_json::json;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
mod compaction;
mod deferral;
mod reload;
mod sections;

#[derive(Default)]
struct Script {
    scripts: Mutex<HashMap<String, String>>,
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
        let prompt = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User && m.origin != Some(MessageOrigin::Generated))
            .map(|m| m.content.clone())
            .unwrap_or_default();
        self.requests.lock().unwrap().push(request);
        let code = self.scripts.lock().unwrap().remove(&prompt);
        Ok(match code {
            Some(code) => ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "js".into(),
                    arguments: json!({"code":code}),
                }],
                ..Default::default()
            },
            None => ModelResponse {
                text: "fixture done".into(),
                ..Default::default()
            },
        })
    }
}
struct Fixture {
    _root: tempfile::TempDir,
    file: PathBuf,
    store: Store,
    live: Arc<LiveConfig>,
    providers: Arc<LiveProviderConfig>,
    mcp: Arc<McpHost>,
    processes: Arc<ProcessTools>,
    agents: Arc<AgentTools>,
    harness: Arc<Harness>,
    script: Arc<Script>,
    session: String,
}
fn config(extra: &str) -> String {
    format!(
        "[model]\nmain='demo/first'\n[approvals]\nconfirm='allow'\n[roles]\nsummary='main'\nchapter='main'\ndecider='main'\napprover='main'\n{extra}"
    )
}
impl Fixture {
    async fn new(text: &str) -> Self {
        Self::new_with_layout(text, false).await
    }
    async fn new_with_layout(text: &str, data_dir: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        Self::open(root, text, data_dir, None).await
    }
    async fn open(
        root: tempfile::TempDir,
        text: &str,
        data_dir: bool,
        session: Option<String>,
    ) -> Self {
        let file = root.path().join("config.toml");
        std::fs::write(&file, text).unwrap();
        let effective = EffectiveConfig::load(LoadOptions {
            workspace: root.path().to_owned(),
            home: None,
            custom_file: Some(file.clone()),
            env: vec![],
            cli: vec![],
        })
        .unwrap();
        let settings = effective.settings().unwrap();
        let store = if data_dir {
            Store::open_data_dir(root.path().join("data")).unwrap()
        } else {
            Store::open(root.path().join("archive.db")).unwrap()
        };
        let session = session.unwrap_or_else(|| store.create_session("fixture").unwrap().id);
        let mut tools = ToolRegistry::new();
        let processes = register_native_tools_with_exec(
            &mut tools,
            root.path(),
            false,
            settings.exec.clone(),
            root.path().join("state"),
        )
        .unwrap();
        let mcp = Arc::new(McpHost::start(&settings.mcp, root.path()).await);
        let providers = Arc::new(LiveProviderConfig::new(effective.clone()).unwrap());
        let live = LiveConfig::new(
            effective,
            providers.clone(),
            mcp.clone(),
            processes.clone(),
            store.clone(),
        )
        .unwrap();
        let repl = Arc::new(
            CodeMode::new(
                root.path().to_owned(),
                Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap()),
            )
            .unwrap(),
        );
        let script = Arc::new(Script::default());
        let mut harness_config = HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            model: "demo/first".into(),
            ..Default::default()
        };
        let live_providers = providers.clone();
        let scripted_provider = script.clone();
        harness_config.provider_resolver = Some(Arc::new(move || {
            (live_providers.main_model(), scripted_provider.clone())
        }));
        live.configure_harness(&mut harness_config);
        let agents = child_agent_host_live(
            store.clone(),
            script.clone(),
            &tools,
            &repl,
            &harness_config,
            root.path(),
            true,
            None,
            settings.agents,
            Arc::new(AgentRegistry::default()),
            processes.clone(),
            providers.clone(),
        )
        .unwrap();
        agents.register(&mut tools).unwrap();
        register_agent_handles(&mut tools, agents.clone()).unwrap();
        repl.register(&mut tools).unwrap();
        live.bind(tools, &agents).unwrap();
        let harness = Arc::new(Harness::new(
            store.clone(),
            script.clone(),
            live.registry(),
            harness_config,
        ));
        Self {
            _root: root,
            file,
            store,
            live,
            providers,
            mcp,
            processes,
            agents,
            harness,
            script,
            session,
        }
    }
    fn write(&self, text: &str) {
        std::fs::write(&self.file, text).unwrap();
    }
    fn code(&self, prompt: &str, code: &str) {
        self.script
            .scripts
            .lock()
            .unwrap()
            .insert(prompt.into(), code.into());
    }
    async fn turn(&self, prompt: &str) {
        let before = self.store.messages(&self.session).unwrap().len();
        let run = self
            .harness
            .prompt(&self.session, prompt, RunOptions::default())
            .await
            .unwrap();
        assert_eq!(run.status, "completed", "{:?}", run.error);
        for message in self
            .store
            .messages(&self.session)
            .unwrap()
            .into_iter()
            .skip(before)
        {
            assert!(
                !message.is_error,
                "scripted tool failed during {prompt:?}: {}",
                message.content
            );
        }
    }
    async fn shutdown(&self) {
        self.agents.shutdown().await.unwrap();
        self.processes.shutdown().await;
        self.mcp.shutdown(Duration::from_secs(2)).await;
    }
    fn notices(&self) -> Vec<String> {
        self.store
            .messages(&self.session)
            .unwrap()
            .into_iter()
            .filter(|m| {
                m.origin == Some(MessageOrigin::Generated) && m.content.starts_with("<harness ")
            })
            .map(|m| m.content)
            .collect()
    }
}

#[tokio::test]
async fn turn_start_applies_sections_and_invalid_file_keeps_old_config() {
    let fixture = Fixture::new(&config("")).await;
    fixture.turn("initial").await;
    let mcp_command = toml::Value::String(env!("CARGO_BIN_EXE_bubblepaw-runner-fake-mcp").into());
    let text = config(&format!("[approvals.tools]\nexec='deny'\n[agents]\nmax_depth=0\nauto_wake=false\nwake_debounce_ms=13\n[exec]\nsync_timeout_secs=0\nmax_jobs=1\n[mcp.servers.fixture]\ncommand={mcp_command}\napproval='none'\n"))
        .replace("demo/first", "alternate/second") + "[providers.alternate]\nkind='scripted'\n";
    fixture.write(&text);
    fixture.code("changed", "try { const r=await tools.exec({command:'printf forbidden'}); if(!JSON.stringify(r).toLowerCase().includes('denied')) throw Error('unexpected '+JSON.stringify(r)); } catch(e) { if(!String(e).toLowerCase().includes('denied')) throw e; } const names=(await tools.$list()).map(t=>t.name); if(!names.includes('mcp__fixture__echo')) throw Error('missing MCP'); try { await tools.agent({task:'blocked',profile:'workspace'}); throw Error('depth allowed'); } catch(e) { if(!String(e).toLowerCase().includes('depth')) throw e; } return 'checked';");
    fixture.turn("changed").await;
    let generation = fixture.providers.snapshot();
    assert_eq!(generation.settings.main_model, "alternate/second");
    assert_eq!(
        generation.settings.roles.summary.models,
        ["alternate/second"]
    );
    assert_eq!(generation.settings.agents.max_depth, Some(0));
    assert!(!generation.settings.agents.auto_wake);
    assert_eq!(generation.settings.exec.sync_timeout_secs, 0);
    assert!(!text.is_empty());
    assert_eq!(fixture.notices().len(), 1);
    assert!(fixture.notices()[0].contains("approval for exec"));
    assert!(fixture.notices()[0].contains("+ mcp__fixture__echo"));
    assert_eq!(
        fixture
            .script
            .requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .model,
        "alternate/second"
    );
    let js_before = serde_json::to_value(fixture.live.registry().definitions()).unwrap();
    fixture.write("[exec]\nmax_output_bytes=0\n[approvals]\nconfirm='allow'\n");
    fixture.turn("invalid").await;
    assert_eq!(fixture.providers.main_model(), "alternate/second");
    assert_eq!(
        serde_json::to_value(fixture.live.registry().definitions()).unwrap(),
        js_before
    );
    assert_eq!(fixture.notices().len(), 2);
    assert!(fixture.notices()[1].contains("exec.max_output_bytes"));
    fixture.turn("same invalid file").await;
    assert_eq!(fixture.notices().len(), 2);
    // Valid new exec defaults take effect on the next call; running jobs are
    // independently covered by exec_jobs' retained-cap/deadline tests.
    fixture.write(&text.replace("exec='deny'", "exec='allow'"));
    fixture.code("exec live", "const r=await tools.exec({command:'sleep 0.2'}); if(r.status!=='backgrounded') throw Error(JSON.stringify(r)); return await wait({handle:r.handle});");
    fixture.turn("exec live").await;
    fixture.write(&config(""));
    fixture.code("removed MCP", "const names=(await tools.$list()).map(t=>t.name); if(names.includes('mcp__fixture__echo')) throw Error('still advertised'); try { await tools.mcp__fixture__echo({text:'removed'}); throw Error('executed removed tool'); } catch(e) { const s=String(e); if(!s.includes('mcp__fixture__echo') || !s.includes('removed')) throw e; } return 'clear removal error';");
    fixture.turn("removed MCP").await;
    fixture.shutdown().await;
}

#[tokio::test]
async fn notified_tool_list_applies_on_turn_start_without_a_config_edit() {
    let fixture = Fixture::new(&config("")).await;
    let tools_file = fixture._root.path().join("tools.json");
    let definition = |name: &str| json!({"name":name,"inputSchema":{"type":"object"}});
    std::fs::write(
        &tools_file,
        json!([definition("echo"), definition("gone"), definition("pid")]).to_string(),
    )
    .unwrap();
    let command = toml::Value::String(env!("CARGO_BIN_EXE_bubblepaw-runner-fake-mcp").into());
    let tools_path = toml::Value::String(tools_file.to_string_lossy().into_owned());
    fixture.write(&config(&format!("[mcp.servers.fixture]\ncommand={command}\nargs=['--tools-file',{tools_path}]\napproval='none'\n")));
    fixture.turn("add server").await;
    let context = || bubblepaw_core::ToolContext {
        session_id: fixture.session.clone(),
        run_id: "probe".into(),
        invocation_id: String::new(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    };
    let pid = fixture
        .live
        .registry()
        .get("mcp__fixture__pid")
        .unwrap()
        .invoke(context(), json!({}))
        .await
        .unwrap()
        .content;
    std::fs::write(
        &tools_file,
        json!([definition("echo"), definition("added"), definition("pid")]).to_string(),
    )
    .unwrap();
    fixture.code(
        "notify",
        "return await tools.mcp__fixture__echo({text:'notify',notify_tools_changed:true});",
    );
    fixture.turn("notify").await;
    assert!(fixture.live.registry().get("mcp__fixture__added").is_none());
    fixture.turn("refresh list").await;
    assert!(fixture.live.registry().get("mcp__fixture__added").is_some());
    assert!(fixture.live.registry().get("mcp__fixture__gone").is_none());
    assert_eq!(fixture.notices().len(), 2);
    assert!(fixture.notices()[1].contains("+ mcp__fixture__added"));
    assert!(fixture.notices()[1].contains("− mcp__fixture__gone"));
    assert_eq!(
        fixture
            .live
            .registry()
            .get("mcp__fixture__pid")
            .unwrap()
            .invoke(context(), json!({}))
            .await
            .unwrap()
            .content,
        pid
    );
    fixture.turn("no duplicate").await;
    assert_eq!(fixture.notices().len(), 2);
    fixture.shutdown().await;
}

#[tokio::test]
async fn memory_files_ten_kib_scripted_session_storage_measurement() {
    let fixture = Fixture::new_with_layout(&config(""), true).await;
    let baseline = Fixture::new_with_layout(&config("[context]\nmemory_files=false\n"), true).await;
    let mut seed = 123_456_789u32;
    let instructions = (0..10 * 1024)
        .map(|index| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            if index % 80 == 79 {
                '\n'
            } else {
                (b'!' + ((seed >> 16) % 90) as u8) as char
            }
        })
        .collect::<String>();
    std::fs::write(fixture._root.path().join("AGENTS.md"), &instructions).unwrap();
    fixture.store.idle_checkpoint().unwrap();
    baseline.store.idle_checkpoint().unwrap();
    let before = fixture.store.storage_stats().unwrap();
    let baseline_before = baseline.store.storage_stats().unwrap();
    fixture.turn("first measured turn").await;
    baseline.turn("first measured turn").await;
    let first = fixture.store.storage_stats().unwrap();
    let baseline_first = baseline.store.storage_stats().unwrap();
    assert_eq!(
        first.transaction_count - before.transaction_count,
        baseline_first.transaction_count - baseline_before.transaction_count + 1,
        "the first turn adds exactly one frozen-snapshot transaction"
    );
    fixture.turn("second measured turn").await;
    baseline.turn("second measured turn").await;
    let after = fixture.store.storage_stats().unwrap();
    let baseline_after = baseline.store.storage_stats().unwrap();
    assert_eq!(
        after.transaction_count - first.transaction_count,
        baseline_after.transaction_count - baseline_first.transaction_count,
        "an unchanged second turn adds zero memory transactions beyond the ordinary turn"
    );
    assert_eq!(after.message_count, baseline_after.message_count);
    assert!(fixture.notices().is_empty());
    let (_, bytes) = fixture
        .store
        .attachment(&fixture.session, "bubblepaw-memory-files-v1")
        .unwrap()
        .unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(saved["files"].as_array().unwrap().len(), 1);
    assert_eq!(saved["files"][0]["content"], instructions);
    assert!(
        fixture
            .store
            .messages(&fixture.session)
            .unwrap()
            .iter()
            .all(|message| !message.content.contains(&instructions))
    );
    {
        let requests = fixture.script.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].system_prompt, requests[1].system_prompt);
        assert!(requests[0].system_prompt.contains(&instructions));
    }
    println!(
        "memory 10KiB scripted two-turn session: {}",
        json!({
            "memory": {"database_bytes":after.database_bytes,"chat_bytes":after.chat_bytes,"wal_bytes":after.wal_bytes,"database_growth":after.database_bytes-before.database_bytes,"page_growth":after.page_bytes-before.page_bytes,"transactions":after.transaction_count-before.transaction_count,"second_turn_transactions":after.transaction_count-first.transaction_count,"attachment_bytes":bytes.len()},
            "baseline": {"database_bytes":baseline_after.database_bytes,"chat_bytes":baseline_after.chat_bytes,"wal_bytes":baseline_after.wal_bytes,"database_growth":baseline_after.database_bytes-baseline_before.database_bytes,"page_growth":baseline_after.page_bytes-baseline_before.page_bytes,"transactions":baseline_after.transaction_count-baseline_before.transaction_count,"second_turn_transactions":baseline_after.transaction_count-baseline_first.transaction_count},
            "additional_memory_transactions":1,"unchanged_turn_additional_memory_transactions":0
        })
    );
    fixture.shutdown().await;
    baseline.shutdown().await;
}

#[tokio::test]
async fn memory_files_freeze_root_and_children_and_deliver_each_edit_once() {
    let fixture = Fixture::new(&config("")).await;
    let path = fixture._root.path().join("AGENTS.md");
    std::fs::write(&path, "ROOT-MEMORY-V1").unwrap();
    fixture.turn("memory warm").await;
    let frozen = fixture
        .script
        .requests
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .system_prompt
        .clone();
    assert!(frozen.contains("ROOT-MEMORY-V1"));
    std::fs::write(&path, "ROOT-MEMORY-V2").unwrap();
    fixture.turn("memory changed").await;
    {
        let requests = fixture.script.requests.lock().unwrap();
        let request = requests.last().unwrap();
        assert_eq!(request.system_prompt, frozen);
        assert!(request.messages.iter().any(|message| {
            message.origin == Some(MessageOrigin::Generated)
                && message.content.contains("Memory files changed")
                && message.content.contains("ROOT-MEMORY-V2")
                && message.content.contains("AGENTS.md")
        }));
    }
    assert_eq!(fixture.notices().len(), 1);
    fixture.turn("memory unchanged").await;
    assert_eq!(fixture.notices().len(), 1);
    fixture.code("spawn memory child", "const child=await tools.agent({task:'memory child unique',profile:'read-only'}); return await wait({handle:child.handle,timeoutMs:10000});");
    fixture.turn("spawn memory child").await;
    {
        let requests = fixture.script.requests.lock().unwrap();
        let child_requests = requests
            .iter()
            .filter(|request| request.session_id != fixture.session)
            .collect::<Vec<_>>();
        assert!(
            !child_requests.is_empty(),
            "child must make a scripted provider request"
        );
        for request in child_requests {
            assert!(request.system_prompt.contains("ROOT-MEMORY-V2"));
            assert!(!request.system_prompt.contains("ROOT-MEMORY-V1"));
        }
        for request in requests
            .iter()
            .filter(|request| request.session_id == fixture.session)
        {
            assert_eq!(request.system_prompt, frozen);
        }
    }
    fixture.shutdown().await;
}

#[tokio::test]
async fn skills_live_host_freezes_catalog_announces_changes_and_reads_external_resources() {
    let external = tempfile::tempdir().unwrap();
    let first = external.path().join("first");
    std::fs::create_dir_all(&first).unwrap();
    std::fs::write(
        first.join("SKILL.md"),
        "---\nname: external-first\ndescription: First external skill\n---\nFIRST-EXTERNAL-BODY\n",
    )
    .unwrap();
    std::fs::write(first.join("helper.txt"), "FIRST-HELPER").unwrap();
    let extra = format!(
        "[skills]\ndirs=[{}]\n",
        toml::Value::String(external.path().to_string_lossy().into_owned())
    );
    let fixture = Fixture::new(&config(&extra)).await;
    let path = json!(first.join("SKILL.md")).to_string();
    let helper = json!(first.join("helper.txt")).to_string();
    fixture.code("read first external skill", &format!("const body=await tools.read({{path:{path}}}); const helper=await tools.read({{path:{helper}}}); if(!body.content.includes('FIRST-EXTERNAL-BODY') || !helper.content.includes('FIRST-HELPER')) throw Error('skill resource missing'); return 'external resources read';"));
    fixture.turn("read first external skill").await;
    let frozen = fixture.script.requests.lock().unwrap()[0]
        .system_prompt
        .clone();
    assert!(frozen.contains("First external skill"));
    assert!(!frozen.contains("FIRST-EXTERNAL-BODY"));
    let added = external.path().join("added");
    std::fs::create_dir_all(&added).unwrap();
    std::fs::write(
        added.join("SKILL.md"),
        "---\nname: external-added\ndescription: Added external skill\n---\nADDED-EXTERNAL-BODY\n",
    )
    .unwrap();
    let path = json!(added.join("SKILL.md")).to_string();
    fixture.code("read added external skill", &format!("const body=await tools.read({{path:{path}}}); if(!body.content.includes('ADDED-EXTERNAL-BODY')) throw Error('added skill missing'); return 'new resource read';"));
    fixture.turn("read added external skill").await;
    assert_eq!(fixture.notices().len(), 1);
    assert!(
        fixture.notices()[0].contains("Skills changed")
            && fixture.notices()[0].contains("external-added")
    );
    fixture.turn("unchanged skills").await;
    assert_eq!(fixture.notices().len(), 1);
    std::fs::remove_dir_all(&added).unwrap();
    fixture.turn("removed whole skill directory").await;
    assert_eq!(fixture.notices().len(), 2);
    assert!(
        fixture.notices()[1].contains("removed") && fixture.notices()[1].contains("external-added")
    );
    let replacement = tempfile::tempdir().unwrap();
    let relocated = replacement.path().join("relocated");
    std::fs::create_dir_all(&relocated).unwrap();
    std::fs::write(relocated.join("SKILL.md"), "---\nname: external-relocated\ndescription: Newly configured skill directory\n---\nRELOCATED-BODY\n").unwrap();
    fixture.write(&config(&format!(
        "[skills]\ndirs=[{}]\n",
        toml::Value::String(replacement.path().to_string_lossy().into_owned())
    )));
    let path = json!(relocated.join("SKILL.md")).to_string();
    fixture.code("changed configured skill directories", &format!("const body=await tools.read({{path:{path}}}); if(!body.content.includes('RELOCATED-BODY')) throw Error('new configured skill missing'); return 'new configured resource read';"));
    fixture.turn("changed configured skill directories").await;
    assert_eq!(fixture.notices().len(), 3);
    assert!(fixture.notices()[2].contains("external-relocated"));
    fixture.write(&config("[skills]\nenabled=false\n"));
    let path = json!(first.join("SKILL.md")).to_string();
    fixture.code("frozen skill remains readable", &format!("const body=await tools.read({{path:{path}}}); if(!body.content.includes('FIRST-EXTERNAL-BODY')) throw Error('frozen skill read revoked'); return 'frozen read retained';"));
    fixture.turn("frozen skill remains readable").await;
    assert!(
        fixture
            .notices()
            .last()
            .unwrap()
            .contains("external-relocated")
    );
    assert!(!fixture.live.check().await.restart_needed);
    for request in fixture.script.requests.lock().unwrap().iter() {
        assert_eq!(request.system_prompt, frozen);
    }
    fixture.shutdown().await;
}

#[tokio::test]
async fn skills_scripted_per_chat_storage_measurement() {
    let fixture = Fixture::new_with_layout(&config(""), true).await;
    let baseline = Fixture::new_with_layout(&config("[skills]\nenabled=false\n"), true).await;
    let folder = fixture._root.path().join(".agents/skills/measured");
    std::fs::create_dir_all(&folder).unwrap();
    let body = "SKILL-BODY-NOT-PERSISTED".repeat(1024);
    std::fs::write(
        folder.join("SKILL.md"),
        format!("---\nname: measured\ndescription: Measured skill catalog\n---\n{body}\n"),
    )
    .unwrap();
    fixture.store.idle_checkpoint().unwrap();
    baseline.store.idle_checkpoint().unwrap();
    let before = fixture.store.storage_stats().unwrap();
    let base_before = baseline.store.storage_stats().unwrap();
    fixture.turn("skills first measured turn").await;
    baseline.turn("skills first measured turn").await;
    let first = fixture.store.storage_stats().unwrap();
    let base_first = baseline.store.storage_stats().unwrap();
    fixture.turn("skills second measured turn").await;
    baseline.turn("skills second measured turn").await;
    let after = fixture.store.storage_stats().unwrap();
    let base_after = baseline.store.storage_stats().unwrap();
    assert_eq!(
        first.transaction_count - before.transaction_count,
        base_first.transaction_count - base_before.transaction_count
    );
    assert_eq!(
        after.transaction_count - first.transaction_count,
        base_after.transaction_count - base_first.transaction_count
    );
    assert_eq!(after.message_count, base_after.message_count);
    assert!(fixture.notices().is_empty());
    let (_, attachment) = fixture
        .store
        .attachment(&fixture.session, "bubblepaw-skills-v1")
        .unwrap()
        .unwrap();
    let (_, base_attachment) = baseline
        .store
        .attachment(&baseline.session, "bubblepaw-skills-v1")
        .unwrap()
        .unwrap();
    assert!(attachment.len() < 1024);
    assert!(!String::from_utf8_lossy(&attachment).contains("SKILL-BODY-NOT-PERSISTED"));
    {
        let requests = fixture.script.requests.lock().unwrap();
        assert_eq!(requests[0].system_prompt, requests[1].system_prompt);
    }
    println!(
        "skills scripted per-chat two-turn session: {}",
        json!({
            "skills":{"database_bytes":after.database_bytes,"chat_bytes":after.chat_bytes,"wal_bytes":after.wal_bytes,"database_growth":after.database_bytes-before.database_bytes,"first_turn_wal_growth":first.wal_bytes-before.wal_bytes,"first_turn_transactions":first.transaction_count-before.transaction_count,"second_turn_transactions":after.transaction_count-first.transaction_count,"second_turn_wal_growth":after.wal_bytes-first.wal_bytes,"attachment_bytes":attachment.len()},
            "disabled":{"database_bytes":base_after.database_bytes,"chat_bytes":base_after.chat_bytes,"wal_bytes":base_after.wal_bytes,"database_growth":base_after.database_bytes-base_before.database_bytes,"first_turn_wal_growth":base_first.wal_bytes-base_before.wal_bytes,"first_turn_transactions":base_first.transaction_count-base_before.transaction_count,"second_turn_transactions":base_after.transaction_count-base_first.transaction_count,"second_turn_wal_growth":base_after.wal_bytes-base_first.wal_bytes,"attachment_bytes":base_attachment.len()},
            "unchanged_turn_additional_skill_transactions":0
        })
    );
    fixture.shutdown().await;
    baseline.shutdown().await;
}

mod tool_inventory;

#[tokio::test]
async fn web_enable_exposure_and_policy_are_live_cache_stays_restart_only() {
    let fixture = Fixture::new(&config("")).await;
    assert!(fixture.live.registry().get("web_fetch").is_none());
    assert!(fixture.agents.profile("research").is_none());
    fixture.write(&config("[web]\nenabled=true\nallow_private=true\nuser_agent='fixture'\nmax_fetches=3\n[web.expose]\nfetch=true\nsearch=false\n"));
    fixture.turn("enable web").await;
    assert!(fixture.live.registry().get("web_fetch").is_some());
    assert!(fixture.live.registry().get("web_search").is_none());
    assert!(fixture.agents.profile("research").is_some());
    assert!(fixture.providers.snapshot().settings.web.allow_private);
    fixture.write(&config("[web]\nenabled=false\n"));
    fixture.turn("disable web").await;
    assert!(fixture.live.registry().get("web_fetch").is_none());
    assert!(fixture.agents.profile("research").is_none());
    fixture.write(&config("").replace("confirm='allow'", "confirm='allow'\nread_only=true"));
    fixture.code("read only", "const c=await tools.agent({task:'read-only child',profile:'read-only'}); const r=await wait({handle:c.handle}); if(r.status!=='completed') throw Error(JSON.stringify(r)); return 'child complete';");
    fixture.turn("read only").await;
    assert!(fixture.live.registry().get("exec").is_none());
    assert!(fixture.live.registry().get("agent").is_some());
    assert!(fixture.agents.profile("workspace").is_none());
    fixture.write(&config(""));
    fixture.turn("writable again").await;
    assert!(fixture.live.registry().get("exec").is_some());
    assert!(fixture.agents.profile("workspace").is_some());
    fixture.shutdown().await;
}

mod storage;

mod deny_reload;
