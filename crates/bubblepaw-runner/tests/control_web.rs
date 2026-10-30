#![cfg(feature = "web")]
//! Local HTTP acceptance with the real host registry and scripted provider.
use anyhow::Result;
use bubblepaw_core::{Harness, HarnessConfig, RunOptions, Store, ToolRegistry};
use bubblepaw_runner::{
    artifacts::ArtifactStore,
    codemode::CodeMode,
    provider::ScriptedProvider,
    question::QuestionTools,
    todo,
    web::{WebConfig, WebServices, serve_with_services},
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn web_snapshot_and_answer_root_question() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let session = store.create_session("web control")?.id;
    let artifacts = Arc::new(ArtifactStore::new(
        directory.path(),
        directory.path().join("artifacts"),
    )?);
    let repl = Arc::new(CodeMode::new(directory.path().into(), artifacts)?);
    let questions = Arc::new(QuestionTools::new(store.clone(), Arc::default(), true));
    let mut tools = ToolRegistry::new();
    todo::register(&mut tools, store.clone())?;
    questions.register(&mut tools)?;
    repl.register(&mut tools)?;
    let harness = Arc::new(Harness::new(
        store.clone(),
        Arc::new(ScriptedProvider::new("done")),
        tools,
        HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            ..Default::default()
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let (stop, receive) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(serve_with_services(
        listener,
        harness.clone(),
        store.clone(),
        WebConfig {
            workspace: directory.path().display().to_string(),
            model: "scripted".into(),
            provider: "demo".into(),
            read_only: false,
        },
        WebServices {
            questions: Some(questions.clone()),
            repl: Some(repl.clone()),
            ..Default::default()
        },
        async {
            let _ = receive.await;
        },
    ));
    harness.start(&session, "ask", RunOptions { script: Some("await tools.todo({items:[{text:'web check',status:'pending'}]}); return await tools.question({text:'Which answer?',options:['First','Second']});".into()), ..Default::default() })?;
    let client = reqwest::Client::new();
    let view = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let view: Value = client
                .get(format!("{base}/api/sessions/{session}"))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            if view["question"]["status"] == "pending" {
                break Ok::<_, anyhow::Error>(view);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert_eq!(view["todo"][0]["text"], "web check");
    assert_eq!(view["session"]["status"], "waiting_for_answer");
    let answer: Value = client
        .post(format!("{base}/api/sessions/{session}/question/answer"))
        .header("Origin", &base)
        .json(&json!({"question_id":view["question"]["id"],"text":"Second"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(answer["live"], true);
    assert_eq!(answer["question"]["answer"], "Second");
    tokio::time::timeout(Duration::from_secs(5), harness.wait_for_idle(&session)).await??;
    let child = store.fork(&session, "child question")?.id;
    store.update_host_state(&child, "", "question", |_| {
        Ok(json!({"id":"child-q","status":"pending","text":"Parent?","parent_session_id":session}))
    })?;
    let refused = client
        .post(format!("{base}/api/sessions/{child}/question/answer"))
        .header("Origin", &base)
        .json(&json!({"question_id":"child-q","text":"bypass"}))
        .send()
        .await?;
    assert!(!refused.status().is_success());
    assert_eq!(
        questions.pending(&child)?.unwrap()["status"],
        "pending",
        "root client must not mutate a child question or bypass its profile"
    );
    assert_eq!(
        store.runs(&session)?.len(),
        1,
        "script questions must not start a model followup"
    );
    let _ = stop.send(());
    task.await??;
    repl.shutdown().await;
    Ok(())
}
