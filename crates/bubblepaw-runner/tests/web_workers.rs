#![cfg(feature = "web")]
//! Scripted local HTTP/SSE visibility acceptance; no model or internet calls.
use anyhow::Result;
use bubblepaw_core::{
    CancellationToken, EventKind, Harness, HarnessConfig, HarnessEvent, Store, ToolContext,
    ToolRegistry,
};
use bubblepaw_runner::{
    agents::{AgentProfile, AgentTools},
    provider::ScriptedProvider,
    web::{WebConfig, WebServices, serve_with_services},
};
use futures_util::{Stream, StreamExt};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::oneshot};

async fn events_until_finished<S, B>(stream: &mut S, run_id: &str) -> Result<Vec<HarnessEvent>>
where
    S: Stream<Item = Result<B, reqwest::Error>> + Unpin,
    B: AsRef<[u8]>,
{
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut buffer = String::new();
        let mut events = Vec::new();
        loop {
            let chunk = stream.next().await.expect("SSE stream remains open")?;
            buffer.push_str(std::str::from_utf8(chunk.as_ref())?);
            while let Some(end) = buffer.find("\n\n") {
                let record: String = buffer.drain(..end + 2).collect();
                let Some(data) = record.lines().find_map(|line| line.strip_prefix("data: ")) else {
                    continue;
                };
                if record.lines().any(|line| line == "event: reset") {
                    continue;
                }
                let event: HarnessEvent = serde_json::from_str(data)?;
                let finished =
                    event.run_id == run_id && matches!(event.kind, EventKind::RunFinished { .. });
                events.push(event);
                if finished {
                    return Ok::<_, anyhow::Error>(events);
                }
            }
        }
    })
    .await?
}

#[tokio::test]
async fn hidden_workers_are_omitted_from_sidebar_and_live_and_replayed_sse() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let parent = store.create_session("visible parent")?;
    let main = Arc::new(Harness::new(
        store.clone(),
        Arc::new(ScriptedProvider::new("visible answer")),
        ToolRegistry::new(),
        HarnessConfig::default(),
    ));
    let profile = AgentProfile {
        name: "internal-web-test".into(),
        description: "Scripted internal task".into(),
        harness: Arc::new(Harness::new(
            store.clone(),
            Arc::new(ScriptedProvider::new("synthetic hidden-worker marker")),
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
        run_id: "host-worker-launch".into(),
        invocation_id: "internal-task".into(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    };
    // A hidden run precedes the visible run, so replay must filter existing journal rows.
    let before_server = manager.start_worker(
        &owner,
        profile.clone(),
        json!({"task":"hidden before server"}),
    )?;
    assert_eq!(
        manager
            .wait(
                &owner.session_id,
                before_server["sessionId"].as_str().unwrap(),
                None
            )
            .await?["status"],
        "completed"
    );
    let first = main.start(&parent.id, "visible replay marker", Default::default())?;
    main.wait_for_idle(&parent.id).await?;
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
        WebServices::default(),
        async {
            let _ = stop.await;
        },
    ));
    let client = reqwest::Client::new();
    let sidebar: Value = client
        .get(format!("{base}/api/sessions"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(sidebar.as_array().unwrap().len(), 1);
    assert_eq!(sidebar[0]["id"], parent.id);
    let response = client
        .get(format!("{base}/api/events"))
        .header("Last-Event-ID", "0")
        .send()
        .await?
        .error_for_status()?;
    let mut replayed = response.bytes_stream();
    let replay = events_until_finished(&mut replayed, &first.id).await?;
    assert!(!replay.is_empty());
    assert!(replay.iter().all(|event| event.session_id == parent.id));
    assert!(!serde_json::to_string(&replay)?.contains("synthetic hidden-worker marker"));
    drop(replayed);

    // A fresh subscriber sees reset, then only the next visible root run.
    let response = client
        .get(format!("{base}/api/events"))
        .send()
        .await?
        .error_for_status()?;
    let mut live = response.bytes_stream();
    let reset = tokio::time::timeout(Duration::from_secs(2), live.next())
        .await?
        .expect("SSE reset")?;
    assert!(std::str::from_utf8(&reset)?.contains("event: reset"));
    let after_server =
        manager.start_worker(&owner, profile, json!({"task":"hidden while connected"}))?;
    assert_eq!(
        manager
            .wait(
                &owner.session_id,
                after_server["sessionId"].as_str().unwrap(),
                None
            )
            .await?["status"],
        "completed"
    );
    let second = main.start(&parent.id, "visible live marker", Default::default())?;
    main.wait_for_idle(&parent.id).await?;
    let events = events_until_finished(&mut live, &second.id).await?;
    assert!(events.iter().all(|event| event.session_id == parent.id));
    assert!(!serde_json::to_string(&events)?.contains("synthetic hidden-worker marker"));
    let sidebar: Value = client
        .get(format!("{base}/api/sessions"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(sidebar.as_array().unwrap().len(), 1);
    for worker in [&before_server, &after_server] {
        let id = worker["sessionId"].as_str().unwrap();
        assert!(store.session(id)?.hidden);
        assert!(
            store
                .messages(id)?
                .iter()
                .any(|message| message.content.contains("synthetic hidden-worker marker"))
        );
        manager.acknowledge_worker(&owner.session_id, id).await?;
    }
    drop(live);
    let _ = shutdown.send(());
    host.await??;
    manager.shutdown().await?;
    Ok(())
}
