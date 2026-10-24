use super::*;
use bubblepaw_core::{HarnessConfig, ModelRequest, ModelResponse, Provider, TextCallback};

struct Hanging;
struct Released(Arc<Notify>);
#[async_trait]
impl Provider for Released {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.0.notified().await;
        Ok(ModelResponse {
            text: "new result".into(),
            ..Default::default()
        })
    }
}
#[async_trait]
impl Provider for Hanging {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        std::future::pending().await
    }
}

#[test]
fn registered_agent_definitions_stay_pinned_until_registry_rebuilt() -> Result<()> {
    let store = Store::in_memory()?;
    let harness = Harness::new(
        store.clone(),
        Arc::new(Hanging),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    let manager = Arc::new(AgentTools::new(store, harness, None).with_limits(Agents {
        max_depth: Some(1),
        ..Default::default()
    }));
    let mut old = ToolRegistry::new();
    manager.register(&mut old)?;
    let before = old.get("agent").unwrap().definition();
    manager.apply_limits(&Agents {
        max_depth: Some(3),
        ..Default::default()
    });
    let after = old.get("agent").unwrap().definition();
    assert_eq!(before.description, after.description);
    assert_eq!(before.input_schema, after.input_schema);
    let mut fresh = ToolRegistry::new();
    manager.register(&mut fresh)?;
    assert_ne!(
        before.description,
        fresh.get("agent").unwrap().definition().description
    );
    Ok(())
}

#[tokio::test]
async fn live_limits_are_shared_and_leave_running_turn_controls_unchanged() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let parent = store.create_session("parent")?;
    let harness = Arc::new(Harness::new(
        store.clone(),
        Arc::new(Hanging),
        ToolRegistry::new(),
        HarnessConfig::default(),
    ));
    let profiles = vec![AgentProfile {
        name: "read-only".into(),
        description: "fixture".into(),
        harness,
        model: None,
        max_rounds: Some(12),
        approval: None,
    }];
    let registry = Arc::new(AgentRegistry::default());
    let manager = AgentTools::from_profiles(store.clone(), profiles.clone())?
        .with_limits(Agents {
            max_depth: Some(2),
            auto_wake: false,
            ..Default::default()
        })
        .with_registry(registry.clone());
    let nested = AgentTools::from_profiles(store, profiles)?.with_registry(registry);
    assert_eq!(nested.limits().max_depth, Some(2));
    let context = ToolContext {
        session_id: parent.id,
        run_id: "fixture".into(),
        invocation_id: "fixture".into(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    };
    let started = manager.start(
        &context,
        json!({"task":"wait","max_tool_calls":3,"timeout":150}),
    )?;
    nested.apply_limits(&Agents {
        max_depth: Some(0),
        max_active_total: Some(1),
        stale_reminder_secs: 0,
        auto_wake: false,
        ..Default::default()
    });
    assert_eq!(manager.limits().max_depth, Some(0));
    assert_eq!(manager.limits().stale_reminder_secs, 0);
    let child = manager.child(&context.session_id, started["handle"].as_str().unwrap())?;
    {
        let turn = child.turn.lock().unwrap();
        assert_eq!(turn.controls.max_tool_calls, Some(3));
        assert_eq!(turn.controls.timeout, Some(150));
        assert!(turn.result.is_none());
    }
    assert!(manager.start(&context, json!({"task":"blocked"})).is_err());
    assert_eq!(
        manager
            .wait(&context.session_id, &child.handle, Some(5000))
            .await?["status"],
        "timed_out"
    );
    manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn stale_notices_repeat_at_interval_and_interaction_restarts_interval() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let parent = store.create_session("parent")?;
    let harness = Harness::new(
        store.clone(),
        Arc::new(Hanging),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    let manager = AgentTools::new(store, harness, None).with_limits(Agents {
        stale_reminder_secs: 7200,
        ..Default::default()
    });
    let context = ToolContext {
        session_id: parent.id,
        run_id: "parent".into(),
        invocation_id: "spawn".into(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    };
    let started = manager.start(&context, json!({"task":"wait"}))?;
    let child = manager.child(&context.session_id, started["handle"].as_str().unwrap())?;
    let began = child.turn.lock().unwrap().last_interaction;
    let interval = Duration::from_secs(7200);
    assert!(
        manager
            .take_reminders_at(
                &context.session_id,
                began + interval - Duration::from_nanos(1)
            )
            .is_empty()
    );
    let notice = manager.take_reminders_at(&context.session_id, began + interval);
    assert_eq!(notice.len(), 1);
    assert!(notice[0].text.contains(&child.handle));
    assert!(notice[0].text.contains(&child.session_id));
    assert!(notice[0].text.contains("running for 7200 seconds"));
    assert!(
        manager
            .take_reminders_at(&context.session_id, began + interval)
            .is_empty()
    );
    assert!(
        manager
            .take_reminders_at(
                &context.session_id,
                began + interval * 2 - Duration::from_nanos(1)
            )
            .is_empty()
    );
    assert_eq!(
        manager
            .take_reminders_at(&context.session_id, began + interval * 2)
            .len(),
        1
    );
    let waiting = WaitInteraction::new(child.clone())?;
    assert!(
        manager
            .take_reminders_at(&context.session_id, began + interval * 10)
            .is_empty()
    );
    drop(waiting);
    let finished_wait = child.turn.lock().unwrap().last_interaction;
    assert!(
        manager
            .take_reminders_at(
                &context.session_id,
                finished_wait + interval - Duration::from_nanos(1)
            )
            .is_empty()
    );
    // Listing is a poll; it resets interaction without consuming a result.
    manager.status(&context.session_id, &json!({}))?;
    let touched = child.turn.lock().unwrap().last_interaction;
    assert!(child.turn.lock().unwrap().last_reminder.is_none());
    assert!(
        manager
            .take_reminders_at(
                &context.session_id,
                touched + interval - Duration::from_nanos(1)
            )
            .is_empty()
    );
    assert_eq!(
        manager
            .take_reminders_at(&context.session_id, touched + interval)
            .len(),
        1
    );
    // A short wait is also interaction, even when it times out.
    assert_eq!(
        manager
            .wait(&context.session_id, &child.handle, Some(0))
            .await?["status"],
        "timeout"
    );
    assert!(child.turn.lock().unwrap().last_reminder.is_none());
    manager.send(&context, json!({"handle":child.handle,"text":"guidance"}))?;
    let touched = child.turn.lock().unwrap().last_interaction;
    assert!(
        manager
            .take_reminders_at(
                &context.session_id,
                touched + interval - Duration::from_nanos(1)
            )
            .is_empty()
    );
    manager.cancel(&context.session_id, &child.handle).await?;
    assert!(
        manager
            .take_reminders_at(&context.session_id, touched + interval * 3)
            .is_empty()
    );
    manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn disabled_stale_notices_never_emit() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let parent = store.create_session("parent")?;
    let harness = Harness::new(
        store.clone(),
        Arc::new(Hanging),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    let manager = AgentTools::new(store, harness, None).with_limits(Agents {
        stale_reminder_secs: 0,
        ..Default::default()
    });
    let context = ToolContext {
        session_id: parent.id,
        run_id: "parent".into(),
        invocation_id: "spawn".into(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    };
    manager.start(&context, json!({"task":"wait"}))?;
    assert!(
        manager
            .take_reminders_at(
                &context.session_id,
                Instant::now() + Duration::from_secs(1_000_000)
            )
            .is_empty()
    );
    manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn stale_cancellation_finalizer_cannot_wait_for_or_change_a_continuation() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let parent = store.create_session("parent")?;
    let release = Arc::new(Notify::new());
    let harness = Harness::new(
        store.clone(),
        Arc::new(Released(release.clone())),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    let manager = AgentTools::new(store.clone(), harness, None);
    let context = ToolContext {
        session_id: parent.id,
        run_id: "parent".into(),
        invocation_id: "spawn".into(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    };
    let started = manager.start(&context, json!({"task":"old turn"}))?;
    let child = manager.child(&context.session_id, started["handle"].as_str().unwrap())?;
    // Two callers capture cancellation of the same old run.
    let first = manager.registry.signal(&store, &context.session_id, false);
    let stale = manager.registry.signal(&store, &context.session_id, false);
    let stale_after_completion = manager.registry.signal(&store, &context.session_id, false);
    AgentRegistry::finish_cancel(first).await;
    let old_run = child.turn.lock().unwrap().run_id.clone();
    manager.send(&context, json!({"handle":child.handle,"text":"new turn"}))?;
    assert_ne!(child.turn.lock().unwrap().run_id, old_run);
    tokio::time::timeout(
        Duration::from_millis(100),
        AgentRegistry::finish_cancel(stale),
    )
    .await?;
    assert_eq!(
        manager.status(&context.session_id, &json!({"handle":child.handle}))?["status"],
        "running"
    );
    assert!(!child.turn.lock().unwrap().stop.is_cancelled());
    // A stale finalizer must also preserve a completed new result.
    release.notify_one();
    assert_eq!(
        manager
            .wait(&context.session_id, &child.handle, Some(1000))
            .await?["status"],
        "completed"
    );
    AgentRegistry::finish_cancel(stale_after_completion).await;
    assert_eq!(
        manager.status(&context.session_id, &json!({"handle":child.handle}))?["status"],
        "idle"
    );
    manager.shutdown().await?;
    Ok(())
}
