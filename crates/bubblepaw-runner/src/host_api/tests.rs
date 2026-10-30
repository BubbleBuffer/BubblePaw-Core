use super::*;
use anyhow::Result;
use base64::{Engine, engine::general_purpose::STANDARD};
use bubblepaw_core::{CancellationToken, HarnessConfig, RunOptions, ToolContext, ToolRegistry};

fn fixture() -> Result<(tempfile::TempDir, HostCtx, String)> {
    let root = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let id = store
        .create_session_with_workspace("fixture", root.path())?
        .id;
    let ctx = HostCtx::for_store(store, root.path());
    Ok((root, ctx, id))
}
async fn call(ctx: &HostCtx, method: &str, params: Value) -> Result<Value> {
    Ok(dispatch(ctx, Profile::Full, method, params).await?)
}

#[tokio::test]
async fn descriptors_gate_every_mutation_before_decoding_and_advertise_methods() -> Result<()> {
    let (_root, ctx, _id) = fixture()?;
    let initialized = call(&ctx, "initialize", json!({})).await?;
    for method in METHODS {
        assert!(
            initialized["features"]
                .as_array()
                .unwrap()
                .contains(&json!(method.name)),
            "{}",
            method.name
        );
        if method.profile > Profile::Read {
            let error = dispatch(&ctx, Profile::Read, method.name, Value::Null)
                .await
                .unwrap_err();
            assert_eq!(
                (error.http_status(), error.rpc_code()),
                (403, -32003),
                "{}",
                method.name
            );
        }
    }
    assert_eq!(
        dispatch(&ctx, Profile::Edit, "session.delete", json!({}))
            .await
            .unwrap_err()
            .http_status(),
        403
    );
    assert_eq!(
        dispatch(
            &ctx,
            Profile::Edit,
            "host.command",
            json!({"text":"/reload"})
        )
        .await
        .unwrap_err()
        .http_status(),
        403
    );
    Ok(())
}

#[tokio::test]
async fn session_aliases_pagination_history_context_and_fork_use_same_store() -> Result<()> {
    let (_root, ctx, id) = fixture()?;
    let opened = call(
        &ctx,
        "session.open",
        json!({"title":"primary","name":"ignored","parent_id":id}),
    )
    .await?;
    assert_eq!(opened["title"], "primary");
    assert_eq!(opened["parent_session_id"], id);
    call(&ctx, "run.start", json!({"session_id":id,"text":"hello"})).await?;
    assert_eq!(
        call(&ctx, "run.wait", json!({"session_id":id})).await?["status"],
        "completed"
    );
    let page = call(&ctx, "session.list", json!({"limit":1})).await?;
    assert_eq!(page["sessions"].as_array().unwrap().len(), 1);
    assert!(page["next_cursor"].is_string());
    let history = call(&ctx, "session.history", json!({"session_id":id,"limit":1})).await?;
    assert_eq!(history["messages"].as_array().unwrap().len(), 1);
    assert!(history["next_seq"].is_number());
    let messages = call(&ctx, "history.messages", json!({"session_id":id,"limit":1})).await?;
    assert_eq!(messages["messages"].as_array().unwrap().len(), 1);
    assert_eq!(messages["has_more"], true);
    assert!(
        call(&ctx, "context.structure", json!({"session_id":id}))
            .await?
            .is_object()
    );
    assert_eq!(
        call(&ctx, "todo.list", json!({"session_id":id})).await?,
        json!([])
    );
    let fork = call(
        &ctx,
        "session.fork",
        json!({"session_id":id,"title":"fork"}),
    )
    .await?;
    assert_ne!(fork["id"], id);
    assert_eq!(
        call(&ctx, "host.command", json!({"text":"ordinary input"})).await?,
        Value::Null
    );
    assert_eq!(
        call(&ctx, "host.command", json!({"text":"/rename title"})).await?,
        json!({"needs_session":true})
    );
    Ok(())
}

#[tokio::test]
async fn offline_services_handle_binary_roundtrips_absence_and_read_views() -> Result<()> {
    let (root, mut ctx, id) = fixture()?;
    ctx.input_files = Some(Arc::new(crate::input_files::InputFiles::new(
        root.path(),
        Default::default(),
    )?));
    let uploaded = call(
        &ctx,
        "attachments.add",
        json!({"session_id":id,"name":"hello.txt","data":STANDARD.encode(b"hello")}),
    )
    .await?;
    let read = call(
        &ctx,
        "attachments.read",
        json!({"session_id":id,"id":uploaded["id"]}),
    )
    .await?;
    assert_eq!(read["data"], STANDARD.encode(b"hello"));
    assert_eq!(
        dispatch(
            &ctx,
            Profile::Read,
            "attachments.read",
            json!({"session_id":id,"id":"missing"})
        )
        .await
        .unwrap_err()
        .http_status(),
        404
    );
    std::fs::write(root.path().join("artifact.txt"), "artifact")?;
    let artifacts = Arc::new(crate::artifacts::ArtifactStore::new(
        root.path(),
        root.path().join("artifacts"),
    )?);
    let artifact = artifacts.capture(&id, "artifact.txt", None, false)?;
    ctx.artifacts = Some(artifacts);
    assert_eq!(
        call(&ctx, "artifacts.list", json!({"session_id":id})).await?[0]["id"],
        artifact.id
    );
    assert_eq!(
        call(
            &ctx,
            "artifacts.read",
            json!({"session_id":id,"id":artifact.id})
        )
        .await?["data"],
        STANDARD.encode(b"artifact")
    );
    assert_eq!(
        call(&ctx, "approval.list", json!({"session_id":id})).await?,
        json!([])
    );
    assert_eq!(
        call(&ctx, "process.list", json!({"session_id":id})).await?,
        json!({"jobs":[]})
    );
    assert_eq!(
        call(&ctx, "environment.inspect", json!({"session_id":id})).await?,
        json!({"environments":[]})
    );
    ctx.config =
        json!({"workspace":"fixture","model":"offline","provider":"scripted","read_only":true});
    assert_eq!(
        call(&ctx, "config.get", json!({})).await?["model"],
        "offline"
    );
    assert_eq!(
        dispatch(&ctx, Profile::Read, "providers.list", json!({}))
            .await
            .unwrap_err()
            .http_status(),
        503
    );
    assert!(call(&ctx, "storage.stats", json!({})).await?.is_object());
    Ok(())
}

#[tokio::test]
async fn subscription_replays_only_visible_owned_events_and_checks_workspace() -> Result<()> {
    let (_root, ctx, id) = fixture()?;
    call(&ctx, "run.start", json!({"session_id":id,"text":"hello"})).await?;
    call(&ctx, "run.wait", json!({"session_id":id})).await?;
    let subscribed = events::subscribe(&ctx, Some(0), None)?;
    assert!(!subscribed.replay.is_empty());
    assert!(subscribed.replay.iter().all(|event| event.session_id == id));
    let scope = crate::session_scope::SessionScope::default();
    let mut other = ctx.clone();
    other.scope = scope;
    assert!(events::subscribe(&other, Some(0), None)?.replay.is_empty());
    let foreign = tempfile::tempdir()?;
    let mut other = ctx.clone();
    other.read_scope = ReadScope::Workspace(foreign.path().into());
    assert_eq!(
        events::subscribe(&other, Some(0), Some(&id))
            .err()
            .unwrap()
            .http_status(),
        409
    );
    Ok(())
}

struct PendingProvider;
#[async_trait::async_trait]
impl bubblepaw_core::Provider for PendingProvider {
    async fn generate(
        &self,
        _request: bubblepaw_core::ModelRequest,
        _text: bubblepaw_core::TextCallback,
        cancel: CancellationToken,
    ) -> Result<bubblepaw_core::ModelResponse> {
        cancel.cancelled().await;
        anyhow::bail!("cancelled fixture")
    }
}

#[tokio::test]
async fn cancel_cascades_to_question_and_running_child() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let parent = store.create_session_with_workspace("parent", root.path())?;
    let provider = Arc::new(PendingProvider);
    let parent_harness = Arc::new(Harness::new(
        store.clone(),
        provider.clone(),
        Default::default(),
        Default::default(),
    ));
    let child_harness = Harness::new(
        store.clone(),
        provider,
        Default::default(),
        Default::default(),
    );
    let agents = Arc::new(crate::agents::AgentTools::new(
        store.clone(),
        child_harness,
        None,
    ));
    let wake = agents.wake_coordinator();
    let questions = Arc::new(crate::question::QuestionTools::new(
        store.clone(),
        wake.clone(),
        true,
    ));
    let mut ctx = HostCtx::new(parent_harness, store.clone(), root.path());
    ctx.agents = Some(agents.clone());
    ctx.questions = Some(questions.clone());
    ctx.wake = Some(wake);
    let admitted = call(
        &ctx,
        "run.start",
        json!({"session_id":parent.id,"text":"wait"}),
    )
    .await?;
    let run = admitted["id"].as_str().unwrap();
    let child = agents.start(
        &ToolContext {
            session_id: parent.id.clone(),
            run_id: run.into(),
            invocation_id: "fixture-child".into(),
            cancellation: CancellationToken::new(),
            approval: Default::default(),
        },
        json!({"task":"wait"}),
    )?;
    store.update_host_state(&parent.id,run,"question",|_| Ok(json!({"id":"pending-question","status":"pending","text":"answer?","options":[],"run_id":run,"parent_session_id":null})))?;
    assert_eq!(
        call(&ctx, "run.cancel", json!({"session_id":parent.id})).await?["requested"],
        true
    );
    assert_eq!(
        store.host_state(&parent.id, "question")?.unwrap()["status"],
        "cancelled"
    );
    let child_result = agents
        .wait(&parent.id, child["handle"].as_str().unwrap(), Some(2000))
        .await?;
    assert_eq!(child_result["status"], "cancelled");
    assert_eq!(
        call(&ctx, "run.wait", json!({"session_id":parent.id})).await?["status"],
        "cancelled"
    );
    agents.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn delete_cancels_owned_processes_and_preserves_another_sessions_job() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let session = store.create_session_with_workspace("delete", root.path())?;
    let other = store.create_session_with_workspace("other", root.path())?;
    let mut registry = ToolRegistry::new();
    let processes = crate::tools::register_native_tools(&mut registry, root.path(), false)?;
    let harness = Arc::new(Harness::new(
        store.clone(),
        Arc::new(crate::provider::ScriptedProvider::new("")),
        registry,
        HarnessConfig {
            approval: bubblepaw_core::ApprovalPolicy {
                confirm: bubblepaw_core::ApprovalMode::Allow,
                ..Default::default()
            },
            ..Default::default()
        },
    ));
    #[cfg(windows)]
    let command = "ping -n 31 127.0.0.1 >nul";
    #[cfg(not(windows))]
    let command = "sleep 30";
    for id in [&session.id, &other.id] {
        let reply = harness
            .call_tool(
                id,
                "exec",
                json!({"command":command,"yieldMs":0,"timeoutMs":60000}),
                RunOptions::default(),
                CancellationToken::new(),
            )
            .await?;
        assert!(!reply.is_error, "{}", reply.content);
    }
    let receipt = store
        .runs(&session.id)?
        .last()
        .cloned()
        .expect("completed tool run");
    let mut ctx = HostCtx::new(harness, store.clone(), root.path());
    ctx.processes = Some(processes.clone());
    assert_eq!(
        processes.jobs(&session.id)?["jobs"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    call(&ctx, "session.delete", json!({"session_id":session.id})).await?;
    assert!(store.session(&session.id).is_err());
    assert!(
        processes.jobs(&session.id)?["jobs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        processes.jobs(&other.id)?["jobs"].as_array().unwrap().len(),
        1
    );

    // Observe the same coordinator that the table reaches through ProcessTools.
    // The callback records delivery without trying to recreate the deleted chat.
    let wake = processes.wake_coordinator();
    wake.configure(true, std::time::Duration::ZERO);
    let (delivered, mut deliveries) = tokio::sync::mpsc::unbounded_channel();
    let registration = wake.register_reminders(
        session.id.clone(),
        Arc::new(move |reminders| {
            delivered
                .send(reminders)
                .map_err(|_| anyhow::anyhow!("wake fixture receiver closed"))?;
            Ok(receipt.clone())
        }),
    );
    let driver = wake.start_driver();
    wake.publish(
        &session.id,
        "fixture:completion",
        "synthetic completion after delete".into(),
    );
    let suppressed =
        tokio::time::timeout(std::time::Duration::from_millis(100), deliveries.recv()).await;
    wake.user_input(&session.id);
    let resumed = tokio::time::timeout(std::time::Duration::from_secs(2), deliveries.recv()).await;
    driver.shutdown().await;
    drop(registration);
    processes.shutdown().await;
    assert!(
        suppressed.is_err(),
        "deleted session received a completion wake"
    );
    let reminders = resumed?.expect("registered wake callback remains alive");
    assert!(
        reminders
            .iter()
            .any(|reminder| reminder.text == "synthetic completion after delete")
    );
    Ok(())
}
