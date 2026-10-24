#![cfg(feature = "web")]
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, Role,
    RunOptions, Store, TextCallback, ToolCall, ToolRegistry,
};
use bubblepaw_runner::{
    artifacts::ArtifactStore,
    codemode::CodeMode,
    provider::{ScriptedProvider, ScriptedStep},
    tools::register_native_tools,
    web::{WebConfig, WebServices, serve_with_services},
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

struct Capture {
    inner: ScriptedProvider,
    requests: Mutex<Vec<ModelRequest>>,
}
#[async_trait]
impl Provider for Capture {
    async fn generate(
        &self,
        request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        assert_eq!(
            request
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["js"]
        );
        self.requests.lock().unwrap().push(request.clone());
        self.inner.generate(request, on_text, cancellation).await
    }
}
fn js(id: &str, environment: &str, code: &str) -> ScriptedStep {
    ScriptedStep::Calls {
        calls: vec![ToolCall {
            id: id.into(),
            name: "js".into(),
            arguments: json!({"environment":environment,"code":code}),
        }],
    }
}

#[tokio::test]
async fn returned_read_reaches_next_model_request_without_publishing_a_dump() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let contents = format!("{}\nend-of-read-proof", "model observation ".repeat(100));
    std::fs::write(directory.path().join("notes.txt"), &contents)?;
    let store = Store::in_memory()?;
    let session = store.create_session("ordinary JS result")?;
    let provider = Arc::new(Capture {
        inner: ScriptedProvider::with_steps(
            vec![js("read", "main", "await tools.read({path:'notes.txt'})")],
            "Read completed",
        ),
        requests: Mutex::default(),
    });
    let mut tools = ToolRegistry::new();
    let processes = register_native_tools(&mut tools, directory.path(), false)?;
    let artifacts = Arc::new(ArtifactStore::new(
        directory.path(),
        directory.path().join("artifacts"),
    )?);
    let repl = Arc::new(CodeMode::new(
        directory.path().to_owned(),
        artifacts.clone(),
    )?);
    repl.register(&mut tools)?;
    let harness = Arc::new(Harness::new(
        store.clone(),
        provider.clone(),
        tools,
        HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            ..Default::default()
        },
    ));
    harness
        .prompt(&session.id, "Read notes", RunOptions::default())
        .await?;
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let result = requests[1]
            .messages
            .iter()
            .find(|message| message.role == Role::Tool)
            .unwrap();
        assert!(!result.is_error, "{}", result.content);
        let data: Value = serde_json::from_str(&result.content)?;
        assert_eq!(data["content"], contents);
    }
    // The same session projection used by the frontend must not contain the
    // model's observation as a published text output.
    let assets = directory.path().join("dist");
    std::fs::create_dir(&assets)?;
    std::fs::write(assets.join("index.html"), "fixture")?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let (stop, receive) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(serve_with_services(
        listener,
        harness,
        store.clone(),
        WebConfig {
            workspace: directory.path().display().to_string(),
            model: "fixture".into(),
            provider: "demo".into(),
            read_only: false,
        },
        assets,
        WebServices {
            repl: Some(repl.clone()),
            artifacts: Some(artifacts),
            input_files: None,
            ..Default::default()
        },
        async {
            let _ = receive.await;
        },
    ));
    let view: Value = reqwest::get(format!("{base}/api/sessions/{}", session.id))
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(
        view["outputs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|record| record["output"]["type"] != "text")
    );
    let _ = stop.send(());
    task.await??;
    repl.shutdown().await;
    processes.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn batched_reads_report_state_changes_and_reset_in_model_context() -> Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("one.txt"), "first contents")?;
    std::fs::write(directory.path().join("two.txt"), "second contents")?;
    let store = Store::in_memory()?;
    let session = store.create_session("batched state notices")?;
    let provider = Arc::new(Capture {
        inner: ScriptedProvider::with_steps(
            vec![
                js(
                    "batch",
                    "main",
                    "const docs = await Promise.all(['one.txt','two.txt'].map(async path => ({path,...await tools.read({path})}))); state.docs=docs; state.temp=true; return docs;",
                ),
                js(
                    "change",
                    "main",
                    "state.docs[0].content='updated'; delete state.temp; return state.docs[0];",
                ),
                js(
                    "reset",
                    "main",
                    "state.reset(); return {keys:Object.keys(state)};",
                ),
            ],
            "done",
        ),
        requests: Mutex::default(),
    });
    let mut tools = ToolRegistry::new();
    let processes = register_native_tools(&mut tools, directory.path(), false)?;
    let artifacts = Arc::new(ArtifactStore::new(
        directory.path(),
        directory.path().join("artifacts"),
    )?);
    let repl = Arc::new(CodeMode::new(directory.path().to_owned(), artifacts)?);
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
            "Read together and reset",
            RunOptions::default(),
        )
        .await?;
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        let results = requests[3]
            .messages
            .iter()
            .filter(|message| message.role == Role::Tool)
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 3);
        let (data, notice) = results[0]
            .content
            .split_once("\n[---]\n")
            .expect("state notice after data");
        let docs: Value = serde_json::from_str(data)?;
        assert_eq!(docs[0]["content"], "first contents");
        assert_eq!(docs[1]["content"], "second contents");
        assert!(notice.contains("state.docs added"), "{notice}");
        assert!(notice.contains("state.temp added"), "{notice}");
        assert!(!notice.contains("first contents"));
        let (_, notice) = results[1].content.split_once("\n[---]\n").unwrap();
        assert!(notice.contains("state.docs changed"), "{notice}");
        assert!(notice.contains("state.temp deleted"), "{notice}");
        let (data, notice) = results[2].content.split_once("\n[---]\n").unwrap();
        let reset: Value = serde_json::from_str(data)?;
        assert_eq!(reset["keys"], json!([]));
        assert!(notice.contains("state reset"), "{notice}");
        assert!(results.iter().all(|message| !message.is_error));
    }
    assert!(
        store
            .outputs(&session.id, 128)?
            .iter()
            .all(|record| record.output["type"] != "text")
    );
    repl.shutdown().await;
    processes.shutdown().await;
    Ok(())
}

#[path = "web_codemode/steering_restart.rs"]
mod steering_restart;
