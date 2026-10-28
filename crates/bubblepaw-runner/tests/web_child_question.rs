#![cfg(feature = "web")]
//! Host users answer a child through the explicit parent/child route. The root
//! route still refuses child answers. Scripted local HTTP; no live model calls.
use anyhow::Result;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, RunOptions, Store, ToolCall, ToolContext,
    ToolRegistry,
};
use bubblepaw_runner::{
    agents::{AgentProfile, AgentTools},
    provider::{ScriptedProvider, ScriptedStep},
    question::QuestionTools,
    wake::WakeCoordinator,
    web::{WebConfig, WebServices, serve_with_services},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::oneshot};

async fn sse_frame<B: AsRef<[u8]>>(
    stream: &mut (impl futures_util::Stream<Item = std::result::Result<B, reqwest::Error>> + Unpin),
    buffer: &mut String,
) -> String {
    loop {
        if let Some(end) = buffer.find("\n\n") {
            let frame = buffer[..end].to_owned();
            buffer.drain(..end + 2);
            return frame;
        }
        let bytes = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("SSE event timed out")
            .expect("SSE stream closed")
            .unwrap();
        buffer.push_str(std::str::from_utf8(bytes.as_ref()).unwrap());
    }
}

async fn assert_parent_sse<B: AsRef<[u8]>>(
    stream: &mut (impl futures_util::Stream<Item = std::result::Result<B, reqwest::Error>> + Unpin),
    buffer: &mut String,
    parent: &str,
    expected: &[Value],
) {
    for expected_event in expected {
        loop {
            let frame = sse_frame(stream, buffer).await;
            assert!(
                !frame.contains("event: reset"),
                "unexpected replay reset: {frame}"
            );
            let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data: ")) else {
                continue;
            };
            let event: Value = serde_json::from_str(data).unwrap();
            if event["session_id"] != parent {
                continue;
            }
            assert_eq!(
                event, *expected_event,
                "SSE changed the parent event envelope"
            );
            let sequence = frame
                .lines()
                .find_map(|line| line.strip_prefix("id: "))
                .unwrap();
            assert_eq!(
                sequence.parse::<u64>().unwrap(),
                event["sequence"].as_u64().unwrap()
            );
            break;
        }
    }
}

#[tokio::test]
async fn user_child_question_endpoint_delivers_and_maps_typed_errors() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let other_workspace = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    store.bind_workspace(directory.path())?;
    let parent = store.create_session("question parent")?;
    let other_parent = store.create_session("wrong parent")?;
    let foreign = store.create_session_with_workspace("foreign parent", other_workspace.path())?;
    let foreign_child =
        store.create_session_with_parent("foreign child", Default::default(), Some(&foreign.id))?;
    let child =
        store.create_session_with_parent("question child", Default::default(), Some(&parent.id))?;
    let questions = Arc::new(QuestionTools::new(
        store.clone(),
        Arc::new(WakeCoordinator::default()),
        true,
    ));
    let mut tools = ToolRegistry::new();
    questions.register(&mut tools)?;
    let main = Arc::new(Harness::new(
        store.clone(),
        Arc::new(ScriptedProvider::with_steps(
            vec![ScriptedStep::Calls {
                calls: vec![ToolCall {
                    id: "http-question".into(),
                    name: "question".into(),
                    arguments: json!({"text":"Child colour?","options":["Blue","Green"]}),
                }],
            }],
            "Child continued.",
        )),
        tools,
        HarnessConfig::default(),
    ));
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
        WebServices {
            questions: Some(questions.clone()),
            ..Default::default()
        },
        async {
            let _ = stop.await;
        },
    ));
    let client = reqwest::Client::new();
    let response = client
        .get(format!("{base}/api/events"))
        .send()
        .await?
        .error_for_status()?;
    let mut live = response.bytes_stream();
    let mut live_buffer = String::new();
    assert!(
        sse_frame(&mut live, &mut live_buffer)
            .await
            .contains("event: reset")
    );
    let replay_from = main.last_sequence();
    main.start(&child.id, "Ask the child question", RunOptions::default())?;
    let pending = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(question) = questions.pending(&child.id)? {
                return Ok::<_, anyhow::Error>(question);
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    let url = |parent: &str, child: &str| {
        format!("{base}/api/sessions/{parent}/children/{child}/question/answer")
    };
    let body = json!({"question_id":pending["id"],"text":"Green with stripes"});
    for (parent_id, child_id, input, expected) in [
        (
            parent.id.as_str(),
            "missing",
            body.clone(),
            reqwest::StatusCode::NOT_FOUND,
        ),
        (
            "missing",
            child.id.as_str(),
            body.clone(),
            reqwest::StatusCode::NOT_FOUND,
        ),
        (
            other_parent.id.as_str(),
            child.id.as_str(),
            body.clone(),
            reqwest::StatusCode::NOT_FOUND,
        ),
        (
            foreign.id.as_str(),
            child.id.as_str(),
            body.clone(),
            reqwest::StatusCode::CONFLICT,
        ),
        (
            parent.id.as_str(),
            foreign_child.id.as_str(),
            body.clone(),
            reqwest::StatusCode::CONFLICT,
        ),
        (
            parent.id.as_str(),
            child.id.as_str(),
            json!({"question_id":"stale","text":"Green"}),
            reqwest::StatusCode::NOT_FOUND,
        ),
        (
            parent.id.as_str(),
            child.id.as_str(),
            json!({"question_id":pending["id"],"text":" "}),
            reqwest::StatusCode::BAD_REQUEST,
        ),
    ] {
        let response = client
            .post(url(parent_id, child_id))
            .json(&input)
            .send()
            .await?;
        assert_eq!(response.status(), expected, "{}", response.text().await?);
        assert_eq!(questions.pending(&child.id)?.unwrap()["id"], pending["id"]);
    }
    let refused = client
        .post(format!("{base}/api/sessions/{}/question/answer", child.id))
        .json(&body)
        .send()
        .await?;
    assert_eq!(refused.status(), reqwest::StatusCode::CONFLICT);
    assert!(questions.pending(&child.id)?.is_some());
    let response = client
        .post(url(&parent.id, &child.id))
        .json(&body)
        .send()
        .await?;
    let status = response.status();
    let accepted: Value = response.json().await?;
    assert_eq!(status, reqwest::StatusCode::OK, "{accepted}");
    assert_eq!(accepted, json!({"answered":true}));
    let run = tokio::time::timeout(Duration::from_secs(5), main.wait_for_idle(&child.id)).await??;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert!(questions.pending(&child.id)?.is_none());
    assert_eq!(store.runs(&child.id)?.len(), 1);
    let parent_events: Vec<Value> = main
        .events_after(replay_from, Some(&parent.id), 1000)?
        .iter()
        .map(serde_json::to_value)
        .collect::<std::result::Result<_, _>>()?;
    assert_eq!(parent_events.len(), 4, "{parent_events:?}");
    assert_eq!(parent_events[0]["type"], "child_run_state_changed");
    assert_eq!(parent_events[0]["state"], "running");
    assert_eq!(parent_events[0]["child_session_id"], child.id);
    assert_eq!(parent_events[0]["run_id"], run.id);
    assert_eq!(parent_events[1]["key"], "child_question");
    assert_eq!(parent_events[1]["value"]["status"], "pending");
    assert_eq!(parent_events[1]["value"]["id"], pending["id"]);
    assert_eq!(parent_events[1]["value"]["text"], "Child colour?");
    assert_eq!(
        parent_events[1]["value"]["options"],
        json!(["Blue", "Green"])
    );
    assert_eq!(parent_events[2]["key"], "child_question_answer");
    assert_eq!(parent_events[2]["value"]["answered_by"], "user");
    assert_eq!(parent_events[2]["value"]["answer"], "Green with stripes");
    assert_eq!(parent_events[3]["state"], "completed");
    assert_eq!(parent_events[3]["run_id"], run.id);
    for event in &parent_events {
        assert_eq!(event["session_id"], parent.id);
        assert_eq!(event["durable"], true);
    }
    assert_parent_sse(&mut live, &mut live_buffer, &parent.id, &parent_events).await;
    let response = client
        .get(format!("{base}/api/events"))
        .header("Last-Event-ID", replay_from.to_string())
        .send()
        .await?
        .error_for_status()?;
    let mut replay = response.bytes_stream();
    assert_parent_sse(&mut replay, &mut String::new(), &parent.id, &parent_events).await;
    drop(live);
    drop(replay);
    assert!(store.runs(&parent.id)?.is_empty());
    assert!(
        store
            .messages(&child.id)?
            .iter()
            .any(|message| message.content.contains("Green with stripes"))
    );
    assert_eq!(
        store
            .host_state(&parent.id, "child_question_answer")?
            .unwrap()["child_session_id"],
        child.id
    );
    let duplicate = client
        .post(url(&parent.id, &child.id))
        .json(&body)
        .send()
        .await?;
    assert_eq!(duplicate.status(), reqwest::StatusCode::CONFLICT);
    assert_eq!(store.runs(&child.id)?.len(), 1);
    let _ = shutdown.send(());
    host.await??;
    Ok(())
}

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
