#![cfg(feature = "web")]
//! A question answer posted to a child session is refused as a 409 and
//! changes nothing. Scripted local HTTP only; no model or internet calls.
use anyhow::Result;
use bubblepaw_core::{CancellationToken, Harness, HarnessConfig, Store, ToolContext, ToolRegistry};
use bubblepaw_runner::{
    agents::{AgentProfile, AgentTools},
    provider::ScriptedProvider,
    web::{WebConfig, WebServices, serve_with_services},
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::{net::TcpListener, sync::oneshot};

#[tokio::test]
async fn answering_a_question_on_a_child_session_is_a_409_and_changes_nothing() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let parent = store.create_session("question parent")?;
    let main = Arc::new(Harness::new(
        store.clone(),
        Arc::new(ScriptedProvider::new("visible answer")),
        ToolRegistry::new(),
        HarnessConfig::default(),
    ));
    let profile = AgentProfile {
        name: "question-child".into(),
        description: "Scripted child for the question refusal".into(),
        harness: Arc::new(Harness::new(
            store.clone(),
            Arc::new(ScriptedProvider::new("child answer")),
            ToolRegistry::new(),
            HarnessConfig::default(),
        )),
        model: None,
        max_rounds: Some(2),
        approval: None,
    };
    let manager = AgentTools::from_profiles(store.clone(), vec![profile.clone()])?;
    let owner = ToolContext {
        session_id: parent.id.clone(),
        run_id: "host-question-launch".into(),
        invocation_id: "child-question".into(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    };
    let started = manager.start_worker(&owner, profile, json!({"task": "child question"}))?;
    let child = started["sessionId"].as_str().unwrap().to_string();
    assert_eq!(
        manager.wait(&owner.session_id, &child, None).await?["status"],
        "completed"
    );
    assert_eq!(
        store.session(&child)?.parent_session_id.as_deref(),
        Some(parent.id.as_str())
    );
    let messages_before = store.messages(&child)?.len();

    let assets = directory.path().join("dist");
    std::fs::create_dir(&assets)?;
    std::fs::write(assets.join("index.html"), "<html>synthetic fixture</html>")?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let (shutdown, stop) = oneshot::channel();
    let host = tokio::spawn(serve_with_services(
        listener,
        main.clone(),
        store.clone(),
        WebConfig {
            workspace: directory.path().display().to_string(),
            model: "scripted".into(),
            provider: "scripted".into(),
            read_only: true,
        },
        assets,
        WebServices::default(),
        async {
            let _ = stop.await;
        },
    ));

    let response = reqwest::Client::new()
        .post(format!("{base}/api/sessions/{child}/question/answer"))
        .json(&json!({"question_id": "question-1", "text": "yes"}))
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
    let body: Value = response.json().await?;
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|error| error.starts_with("Child questions must be answered")),
        "unexpected body: {body}"
    );
    assert_eq!(store.messages(&child)?.len(), messages_before);

    let _ = shutdown.send(());
    host.await??;
    manager.shutdown().await?;
    Ok(())
}
