//! A persisted JavaScript conversation resumes under a native host default.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ExecutionMode, Harness, HarnessConfig, ModelRequest, ModelResponse,
    Provider, Role, RunOptions, Store, TextCallback, ToolCall, ToolRegistry,
};
use bubblepaw_runner::{
    artifacts::ArtifactStore,
    codemode::CodeMode,
    execution_mode::configure_prompts,
    tools::{ProcessTools, register_native_tools},
};
use serde_json::json;
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

struct Script {
    mode: ExecutionMode,
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
        let names: Vec<_> = request
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        if self.mode == ExecutionMode::Js {
            assert_eq!(names, ["js"]);
            assert_eq!(request.system_prompt, "default JS instructions");
        } else {
            assert!(names.contains(&"read"));
            assert!(!names.contains(&"js"));
            assert_eq!(request.system_prompt, "default native instructions");
        }
        let first = request
            .messages
            .last()
            .is_some_and(|message| message.role == Role::User);
        self.requests.lock().unwrap().push(request);
        Ok(if first {
            ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: if self.mode == ExecutionMode::Js {
                        "js"
                    } else {
                        "read"
                    }
                    .into(),
                    arguments: if self.mode == ExecutionMode::Js {
                        json!({"code":"return await tools.read({path:'sample.txt'});"})
                    } else {
                        json!({"path":"sample.txt"})
                    },
                }],
                ..Default::default()
            }
        } else {
            ModelResponse {
                text: "accepted resumed request".into(),
                ..Default::default()
            }
        })
    }
}
fn tools(workspace: &Path) -> Result<(ToolRegistry, Arc<CodeMode>, Arc<ProcessTools>)> {
    let mut tools = ToolRegistry::new();
    let processes = register_native_tools(&mut tools, workspace, false)?;
    let repl = Arc::new(CodeMode::new(
        workspace.to_owned(),
        Arc::new(ArtifactStore::new(workspace, workspace.join("artifacts"))?),
    )?);
    repl.register(&mut tools)?;
    Ok((tools, repl, processes))
}
fn harness(store: Store, tools: ToolRegistry, script: Arc<Script>, mode: ExecutionMode) -> Harness {
    let mut config = HarnessConfig {
        execution_mode: mode,
        ..Default::default()
    };
    configure_prompts(
        &mut config,
        store.clone(),
        "default native instructions".into(),
        "default JS instructions".into(),
    );
    Harness::new(store, script, tools, config)
}

#[tokio::test]
async fn js_chat_resumes_with_js_requests_under_native_configuration() -> Result<()> {
    let root = tempfile::tempdir()?;
    let data = root.path().join("data");
    std::fs::write(root.path().join("sample.txt"), "persisted JS chat")?;
    let session;
    {
        let store = Store::open_data_dir(&data)?;
        session = store.create_session("JS before default change")?.id;
        let (tools, repl, processes) = tools(root.path())?;
        let script = Arc::new(Script {
            mode: ExecutionMode::Js,
            requests: Mutex::new(Vec::new()),
        });
        let harness = harness(store, tools, script, ExecutionMode::Js);
        let run = harness
            .prompt(&session, "read before restart", RunOptions::default())
            .await?;
        assert_eq!(run.status, "completed", "{:?}", run.error);
        repl.shutdown().await;
        processes.shutdown().await;
    }
    let store = Store::open_data_dir(&data)?;
    let (tools, repl, processes) = tools(root.path())?;
    let script = Arc::new(Script {
        mode: ExecutionMode::Js,
        requests: Mutex::new(Vec::new()),
    });
    assert_eq!(
        HarnessConfig::default().execution_mode,
        ExecutionMode::Native
    );
    let harness = harness(
        store.clone(),
        tools,
        script.clone(),
        HarnessConfig::default().execution_mode,
    );
    let definitions = harness.effective_tool_definitions(&session, &RunOptions::default())?;
    assert_eq!(
        definitions
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["js"]
    );
    let run = harness
        .prompt(&session, "read after restart", RunOptions::default())
        .await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(run.text.as_deref(), Some("accepted resumed request"));
    assert_eq!(script.requests.lock().unwrap().len(), 2);
    assert!(
        store
            .messages(&session)?
            .iter()
            .filter(|message| message.role == Role::Tool)
            .all(|message| !message.is_error)
    );
    repl.shutdown().await;
    processes.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn fresh_default_chat_uses_native_declarations_and_instructions() -> Result<()> {
    let root = tempfile::tempdir()?;
    std::fs::write(root.path().join("sample.txt"), "default native chat")?;
    let store = Store::in_memory()?;
    let session = store.create_session("new default")?.id;
    let (tools, repl, processes) = tools(root.path())?;
    let script = Arc::new(Script {
        mode: ExecutionMode::Native,
        requests: Mutex::new(Vec::new()),
    });
    let harness = harness(
        store,
        tools,
        script.clone(),
        HarnessConfig::default().execution_mode,
    );
    let run = harness
        .prompt(&session, "read default", RunOptions::default())
        .await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(script.requests.lock().unwrap().len(), 2);
    repl.shutdown().await;
    processes.shutdown().await;
    Ok(())
}
