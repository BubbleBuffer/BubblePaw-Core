use super::*;

#[cfg(feature = "web")]
#[tokio::test]
async fn http_sidebar_keeps_archived_root_and_child_packed_with_accurate_summaries()
-> anyhow::Result<()> {
    use bubblepaw_core::{Harness, HarnessConfig, RunOptions, ToolRegistry};
    use bubblepaw_runner::{
        provider::ScriptedProvider,
        web::{WebConfig, WebServices, serve_with_services},
    };
    use std::sync::Arc;
    let root = tempfile::tempdir()?;
    let data = root.path().join("data");
    let store = Store::open_data_dir(&data)?;
    let parent = store.create_session("Archived parent")?;
    let child = store.create_session_with_parent(
        "Archived child",
        ContextStrategy::Lcm,
        Some(&parent.id),
    )?;
    let harness = Arc::new(Harness::new(
        store.clone(),
        Arc::new(ScriptedProvider::new("cold reply")),
        ToolRegistry::new(),
        HarnessConfig::default(),
    ));
    for session in [&parent, &child] {
        harness
            .prompt(&session.id, "synthetic request", RunOptions::default())
            .await?;
        harness.wait_for_idle(&session.id).await?;
    }
    let expected = store.visible_session_summaries(20)?;
    assert!(store.archive_chat(&parent.id)?);
    let path = data.join("chats").join(format!("{}.db.zst", parent.id));
    let archived_bytes = fs::read(&path)?;
    let assets = root.path().join("dist");
    fs::create_dir(&assets)?;
    fs::write(assets.join("index.html"), "fixture")?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let (stop, receive) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(serve_with_services(
        listener,
        harness,
        store.clone(),
        WebConfig {
            workspace: root.path().display().to_string(),
            model: "scripted".into(),
            provider: "demo".into(),
            read_only: false,
        },
        assets,
        WebServices::default(),
        async {
            let _ = receive.await;
        },
    ));
    let client = reqwest::Client::new();
    // Both first materialization and the cached sidebar preserve cold storage.
    for _ in 0..2 {
        let listed: Vec<Value> = client
            .get(format!("{base}/api/sessions"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        assert_eq!(listed.len(), 2);
        for record in &expected {
            let item = listed
                .iter()
                .find(|item| item["id"] == record.session.id)
                .unwrap();
            assert_eq!(item["title"], record.session.title);
            assert_eq!(item["created_at"], record.session.created_at);
            assert_eq!(item["message_count"], record.message_count);
            let message = record.last_message.as_ref().unwrap();
            assert_eq!(item["last_message_preview"], message.content);
            let run = record.latest_run.as_ref().unwrap();
            assert_eq!(item["status"], run.status);
            let updated = record
                .session
                .updated_at
                .clone()
                .max(message.created_at.clone())
                .max(run.ended_at.as_ref().unwrap_or(&run.started_at).clone());
            assert_eq!(item["updated_at"], updated);
        }
        assert_eq!(fs::read(&path)?, archived_bytes);
        assert!(!path.with_extension("").exists());
        for session in [&parent, &child] {
            assert!(store.active_session_metadata(&session.id)?.is_none());
        }
    }
    stop.send(()).unwrap();
    task.await??;
    Ok(())
}
