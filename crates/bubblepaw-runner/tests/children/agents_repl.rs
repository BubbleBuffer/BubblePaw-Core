use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, Role,
    RunOptions, Store, TextCallback, ToolCall, ToolRegistry, Usage,
};
use bubblepaw_runner::{
    agent_handles::register_agent_handles, agents::AgentTools, artifacts::ArtifactStore,
    codemode::CodeMode, tools::register_native_tools,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

struct FamilyProvider {
    requests: Mutex<Vec<ModelRequest>>,
}
#[async_trait]
impl Provider for FamilyProvider {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        assert_eq!(request.tools.len(), 1);
        assert_eq!(request.tools[0].name, "js");
        if request.messages.last().is_some_and(|message| {
            message.role == Role::User && message.content == "PARENT HISTORY MUST STAY PRIVATE"
        }) {
            self.requests.lock().unwrap().push(request);
            return Ok(ModelResponse {
                text: "private parent context recorded".into(),
                ..Default::default()
            });
        }
        let parent = request
            .messages
            .iter()
            .any(|message| message.role == Role::User && message.content == "parent proof");
        let has_result = request
            .messages
            .iter()
            .any(|message| message.role == Role::Tool);
        if !parent {
            assert!(!request.tools[0].description.contains("tools.agent("));
            assert!(request.messages.iter().any(|message| {
                message.content.contains("Explain these supplied files")
                    && message.content.contains("first document")
                    && message.content.contains("second document")
            }));
            assert!(
                !request
                    .messages
                    .iter()
                    .any(|message| message.content.contains("PARENT HISTORY MUST STAY PRIVATE"))
            );
        }
        self.requests.lock().unwrap().push(request);
        if has_result {
            return Ok(ModelResponse {
                text: if parent {
                    "delegation finished"
                } else {
                    "Child explanation from supplied files"
                }
                .into(),
                usage: Some(Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
        let code = if parent {
            "state.parent='private'; const docs=await Promise.all(['one.txt','two.txt'].map(async path=>({path,...await tools.read({path})}))); state.worker=await tools.agent({task:'Explain these supplied files',context:docs}); const result=await wait({handle:state.worker.handle}); if(result.status!=='completed') throw Error(JSON.stringify(result)); text(result.text); return result;"
        } else {
            "if(state.parent!==undefined) throw Error('parent state leaked'); const read=await tools.read({path:'one.txt'}); text('child-only publication'); return {read:read.content,canEdit:'edit' in tools?typeof tools.edit:'undefined',canWrite:'write' in tools?typeof tools.write:'undefined',canSpawn:'agent' in tools?typeof tools.agent:'undefined'};"
        };
        Ok(ModelResponse {
            tool_calls: vec![ToolCall {
                id: uuid::Uuid::new_v4().to_string(),
                name: "js".into(),
                arguments: json!({"code":code}),
            }],
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn parent_repl_delegates_waits_and_publishes_child_answer_with_isolated_context() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("one.txt"), "first document")?;
    std::fs::write(directory.path().join("two.txt"), "second document")?;
    let archive = directory.path().join("archive.sqlite");
    let store = Store::open(&archive)?;
    let session = store.create_session("PARENT HISTORY MUST STAY PRIVATE")?;
    let provider = Arc::new(FamilyProvider {
        requests: Mutex::default(),
    });
    let mut tools = ToolRegistry::new();
    let processes = register_native_tools(&mut tools, directory.path(), false)?;
    let artifacts = Arc::new(ArtifactStore::new(
        directory.path(),
        directory.path().join("artifacts"),
    )?);
    let repl = Arc::new(CodeMode::new(directory.path().to_owned(), artifacts)?);
    let mut child_tools = tools.read_only();
    repl.register(&mut child_tools)?;
    let child = Harness::new(
        store.clone(),
        provider.clone(),
        child_tools,
        HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            ..Default::default()
        },
    );
    let agents = Arc::new(
        AgentTools::new(store.clone(), child, None)
            .with_repl(repl.clone())
            .with_processes(processes.clone()),
    );
    agents.register(&mut tools)?;
    register_agent_handles(&mut tools, agents.clone())?;
    repl.register(&mut tools)?;
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        tools,
        HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            ..Default::default()
        },
    );
    harness
        .prompt(
            &session.id,
            "PARENT HISTORY MUST STAY PRIVATE",
            RunOptions::default(),
        )
        .await?;
    harness
        .prompt(&session.id, "parent proof", RunOptions::default())
        .await?;
    let messages = store.messages(&session.id)?;
    let reply = messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .unwrap();
    assert!(!reply.is_error, "{}", reply.content);
    let result: Value = serde_json::from_str(reply.content.split("\n[---]\n").next().unwrap())?;
    assert_eq!(result["status"], "completed");
    assert_eq!(result["text"], "Child explanation from supplied files");
    assert!(result["usage"]["input_tokens"].as_u64().unwrap() > 0);
    let child_id = result["sessionId"].as_str().unwrap().to_owned();
    assert_ne!(child_id, session.id);
    assert!(store.pending_effects(&session.id)?.is_empty());
    let parent_outputs = store.outputs(&session.id, 128)?;
    assert!(
        parent_outputs
            .iter()
            .any(|output| output.output["text"] == "Child explanation from supplied files")
    );
    assert!(
        !parent_outputs
            .iter()
            .any(|output| output.output["text"] == "child-only publication")
    );
    assert!(
        store
            .outputs(&child_id, 128)?
            .iter()
            .any(|output| output.output["text"] == "child-only publication")
    );
    assert!(
        repl.status(&child_id)["environments"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 5);
        let child_request = requests
            .iter()
            .find(|request| {
                request
                    .messages
                    .iter()
                    .any(|message| message.role == Role::Tool)
                    && !request
                        .messages
                        .iter()
                        .any(|message| message.content == "parent proof")
            })
            .unwrap();
        let content = &child_request
            .messages
            .iter()
            .find(|message| message.role == Role::Tool)
            .unwrap()
            .content;
        let read: Value = serde_json::from_str(content)?;
        assert_eq!(read["canEdit"], "undefined");
        assert_eq!(read["canWrite"], "undefined");
        assert_eq!(read["canSpawn"], "undefined");
    }
    agents.shutdown().await?;
    repl.shutdown().await;
    processes.shutdown().await;
    drop(harness);
    drop(agents);
    drop(provider);
    drop(store);
    let reopened = Store::open(&archive)?;
    assert!(
        reopened
            .messages(&child_id)?
            .iter()
            .any(|message| message.content == "Child explanation from supplied files")
    );
    Ok(())
}

struct CellProvider;
#[async_trait]
impl Provider for CellProvider {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let (index, input) = request
            .messages
            .iter()
            .enumerate()
            .rev()
            .find(|(_, message)| message.role == Role::User)
            .unwrap();
        if input.content == "blocking child" {
            cancellation.cancelled().await;
            anyhow::bail!("synthetic child cancelled");
        }
        if let Some(code) = input.content.strip_prefix("CELL:")
            && !request.messages[index + 1..]
                .iter()
                .any(|message| message.role == Role::Tool)
        {
            return Ok(ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "js".into(),
                    arguments: json!({"code": code}),
                }],
                ..Default::default()
            });
        }
        Ok(ModelResponse {
            text: "parent settled".into(),
            ..Default::default()
        })
    }
}

struct ControlFixture {
    _directory: tempfile::TempDir,
    store: Store,
    session: String,
    harness: Harness,
    registry: ToolRegistry,
    agents: Arc<AgentTools>,
    repl: Arc<CodeMode>,
    processes: Arc<bubblepaw_runner::tools::ProcessTools>,
}
impl ControlFixture {
    fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let store = Store::in_memory()?;
        let session = store.create_session("parent controls")?.id;
        let provider = Arc::new(CellProvider);
        let mut registry = ToolRegistry::new();
        let processes = register_native_tools(&mut registry, directory.path(), false)?;
        let repl = Arc::new(CodeMode::new(
            directory.path().to_owned(),
            Arc::new(ArtifactStore::new(
                directory.path(),
                directory.path().join("artifacts"),
            )?),
        )?);
        let mut child_tools = registry.read_only();
        repl.register(&mut child_tools)?;
        let child = Harness::new(
            store.clone(),
            provider.clone(),
            child_tools,
            HarnessConfig {
                execution_mode: bubblepaw_core::ExecutionMode::Js,
                ..Default::default()
            },
        );
        let agents = Arc::new(
            AgentTools::new(store.clone(), child, None)
                .with_repl(repl.clone())
                .with_processes(processes.clone()),
        );
        agents.register(&mut registry)?;
        register_agent_handles(&mut registry, agents.clone())?;
        repl.register(&mut registry)?;
        let harness = Harness::new(
            store.clone(),
            provider,
            registry.clone(),
            HarnessConfig {
                execution_mode: bubblepaw_core::ExecutionMode::Js,
                ..Default::default()
            },
        );
        Ok(Self {
            _directory: directory,
            store,
            session,
            harness,
            registry,
            agents,
            repl,
            processes,
        })
    }
    async fn close(&self) -> Result<()> {
        self.agents.shutdown().await?;
        self.repl.shutdown().await;
        self.processes.shutdown().await;
        Ok(())
    }
}

#[tokio::test]
async fn shared_js_wait_and_cancel_route_agent_process_and_timer_handles() -> Result<()> {
    let fixture = ControlFixture::new()?;
    #[cfg(windows)]
    let command = "echo process-ok";
    #[cfg(not(windows))]
    let command = "printf 'process-ok\\n'";
    let code = format!(
        "const job=await tools.agent({{task:'blocking child'}}); const timed=await wait({{handle:job.handle,timeoutMs:0}}); if(timed.status!=='timeout') throw Error(JSON.stringify(timed)); const cancelled=await tools.cancel({{handle:job.handle}}); const timer=await wait({{ms:0}}); const p=await tools.exec({}); const process=await wait({{handle:p.handle}}); const output=await tools.poll({{handle:p.handle}}); return {{cancelled,timer,process,output}};",
        json!({"command":command,"yieldMs":0})
    );
    fixture
        .harness
        .prompt(
            &fixture.session,
            format!("CELL:{code}"),
            RunOptions::default(),
        )
        .await?;
    let messages = fixture.store.messages(&fixture.session)?;
    let reply = messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .unwrap();
    assert!(!reply.is_error, "{}", reply.content);
    let result: Value = serde_json::from_str(&reply.content)?;
    assert_eq!(result["cancelled"]["status"], "cancelled");
    assert_eq!(result["process"]["status"], "exited");
    assert!(
        result["output"]["stdout"]
            .as_str()
            .unwrap()
            .contains("process-ok")
    );
    assert!(fixture.store.pending_effects(&fixture.session)?.is_empty());
    fixture.close().await
}

#[tokio::test]
async fn explicit_host_interrupt_backgrounds_child_wait_and_parent_stop_cancels_child_after_parent_settles()
-> Result<()> {
    let fixture = ControlFixture::new()?;
    fixture.harness.start(&fixture.session,"CELL:state.job=await tools.agent({task:'blocking child'}); return await wait({handle:state.job.handle});",RunOptions::default())?;
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if fixture.repl.status(&fixture.session)["environments"]
                .as_array()
                .unwrap()
                .iter()
                .any(|env| env["waiting"] == 1)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await?;
    let child_id = fixture
        .store
        .list_sessions()?
        .into_iter()
        .find(|session| session.id != fixture.session)
        .unwrap()
        .id;
    fixture
        .harness
        .steer(&fixture.session, "inspect while child waits")?;
    assert!(fixture.registry.interrupt_waits(&fixture.session) > 0);
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        fixture.harness.wait_for_idle(&fixture.session),
    )
    .await??;
    assert_eq!(
        fixture.repl.status(&fixture.session)["environments"][0]["status"],
        "background"
    );
    assert_eq!(fixture.store.runs(&child_id)?[0].status, "running");
    assert!(fixture.harness.cancel(&fixture.session));
    tokio::time::timeout(std::time::Duration::from_secs(3), fixture.agents.shutdown()).await??;
    assert_eq!(fixture.store.runs(&child_id)?[0].status, "cancelled");
    assert_eq!(
        fixture
            .store
            .messages(&child_id)?
            .iter()
            .filter(|message| message.role == Role::User)
            .count(),
        1
    );
    assert!(
        fixture.repl.status(&child_id)["environments"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    fixture.close().await
}

#[path = "agents_repl/wait_and_cancel.rs"]
mod wait_and_cancel;
