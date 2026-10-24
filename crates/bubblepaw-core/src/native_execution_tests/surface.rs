use super::*;

#[test]
fn native_registry_projection_preserves_hosts_and_default_model_tools() -> Result<()> {
    let mut tools = ToolRegistry::new();
    tools.register_host(Arc::new(Capability::new("read")))?;
    tools.register(Arc::new(Capability::new("custom")))?;
    tools.register(Arc::new(JsBridge))?;
    assert_eq!(
        names(tools.with_execution_mode(ExecutionMode::Js).definitions()),
        ["js"]
    );
    let native = tools.with_execution_mode(ExecutionMode::Native);
    assert_eq!(
        names(native.definitions()),
        ["call_tool", "custom", "tool_search", "read"]
    );
    assert!(native.get_host("read").is_some());
    assert!(native.get_model("read").is_some());
    assert!(native.get_model("js").is_none());
    assert_eq!(native.restricted(&["read".into()]).capabilities().len(), 1);
    assert!(
        native
            .restricted_capabilities(&[])
            .get_model("read")
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn effective_definitions_match_model_requests_with_modes_help_policy_and_restrictions()
-> Result<()> {
    for mode in [ExecutionMode::Js, ExecutionMode::Native] {
        let store = Store::in_memory()?;
        let session = store.create_session("definitions")?;
        let provider = Arc::new(RecordingProvider {
            code_surface: true,
            ..Default::default()
        });
        let mut tools = ToolRegistry::new();
        tools.register_host(Arc::new(Capability::new("read")))?;
        tools.register_host(Arc::new(Capability::new("list")))?;
        tools.register(Arc::new(JsBridge))?;
        let harness = Harness::new(
            store,
            provider.clone(),
            tools,
            HarnessConfig {
                execution_mode: mode,
                ..Default::default()
            },
        );
        let options = RunOptions {
            tool_names: Some(vec!["js".into(), "read".into()]),
            approval: Some(ApprovalPolicy {
                deny: vec!["read".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let expected = harness.effective_tool_definitions(&session.id, &options)?;
        if let Some(js) = expected.iter().find(|tool| tool.name == "js") {
            assert_eq!(js.description, "read");
        }
        let run = harness.prompt(&session.id, "inspect", options).await?;
        assert_eq!(run.status, "completed");
        let requests = provider.requests.lock().unwrap();
        assert_eq!(
            serde_json::to_value(&expected)?,
            serde_json::to_value(&requests[0].tools)?
        );
        assert_eq!(
            names(expected),
            if mode == ExecutionMode::Js {
                vec!["js"]
            } else {
                vec!["call_tool", "tool_search", "list", "read"]
            }
        );
        assert_eq!(
            serde_json::to_value(harness.effective_tool_definitions(
                &session.id,
                &RunOptions {
                    max_tool_calls: Some(0),
                    ..Default::default()
                }
            )?)?,
            serde_json::to_value(&requests[0].tools)?,
            "a later budget changes admission without replacing the chat's declarations"
        );
    }
    Ok(())
}

#[tokio::test]
async fn single_tool_call_preserves_results_journal_effects_and_pending() -> Result<()> {
    for pending in [false, true] {
        let capability = Arc::new(Capability {
            pending,
            ..Capability::new("read")
        });
        let (store, session, harness, provider) = fixture(native_config(), capability.clone())?;
        let result = harness
            .call_tool(
                &session.id,
                "read",
                json!({"value":"hello"}),
                RunOptions::default(),
                CancellationToken::new(),
            )
            .await?;
        assert_eq!(result.content, "hello");
        assert!(!result.is_error);
        assert_eq!(result.pending, pending);
        assert_eq!(capability.calls.load(Ordering::SeqCst), 1);
        assert!(provider.requests.lock().unwrap().is_empty());
        let messages = store.messages(&session.id)?;
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::Assistant);
        assert_eq!(messages[1].role, Role::Tool);
        assert_eq!(
            messages[0].tool_calls[0].id,
            messages[1].tool_call_id.as_deref().unwrap()
        );
        let events = harness.events_after(0, Some(&session.id), 100)?;
        assert!(events.iter().any(|event|matches!(&event.kind,EventKind::ToolFinished { result,.. } if result.pending==pending && result.content=="hello")));
        assert_eq!(
            store.pending_effects(&session.id)?.len(),
            usize::from(pending)
        );
        assert_eq!(harness.wait_for_idle(&session.id).await?.model, "tool");
    }
    Ok(())
}

#[tokio::test]
async fn single_tool_call_leaves_prequeued_user_input_pending() -> Result<()> {
    let (store, session, harness, provider) =
        fixture(native_config(), Arc::new(Capability::new("read")))?;
    let queued = harness.followup(&session.id, "answer this later")?;
    let result = harness
        .call_tool(
            &session.id,
            "read",
            json!({"value":"hello"}),
            RunOptions::default(),
            CancellationToken::new(),
        )
        .await?;
    assert_eq!(result.content, "hello");
    assert!(!result.is_error);
    let inputs = store.queued_inputs(&session.id)?;
    assert!(inputs.iter().any(|input| {
        input.id == queued.id && input.state == "pending" && input.text == "answer this later"
    }));
    assert!(
        store
            .messages(&session.id)?
            .iter()
            .all(|message| message.role != Role::User)
    );
    assert!(provider.requests.lock().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn single_tool_call_waits_for_its_own_run_when_another_run_starts() -> Result<()> {
    struct BlockingProvider {
        entered: Notify,
        calls: AtomicUsize,
    }
    #[async_trait]
    impl Provider for BlockingProvider {
        async fn generate(
            &self,
            _: ModelRequest,
            _: TextCallback,
            cancellation: CancellationToken,
        ) -> Result<ModelResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            cancellation.cancelled().await;
            anyhow::bail!("Second run cancelled")
        }
    }
    let store = Store::in_memory()?;
    let session = store.create_session("separate completion")?;
    let provider = Arc::new(BlockingProvider {
        entered: Notify::new(),
        calls: AtomicUsize::new(0),
    });
    let mut tools = ToolRegistry::new();
    tools.register_host(Arc::new(Capability::new("read")))?;
    let harness = Harness::new(store, provider.clone(), tools, native_config());
    let mut events = harness.subscribe();
    let first = harness.call_tool(
        &session.id,
        "read",
        json!({"value":"first result"}),
        RunOptions::default(),
        CancellationToken::new(),
    );
    tokio::pin!(first);
    // Admit the first run, then deliberately keep its caller unpolled while
    // the event consumer starts another run on the same session.
    poll_fn(|context| {
        assert!(first.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
    let first_run_id = loop {
        let event = events.recv().await?;
        if matches!(event.kind, EventKind::RunFinished { .. }) {
            break event.run_id;
        }
    };
    let second = harness.start(&session.id, "second request", RunOptions::default())?;
    assert_ne!(first_run_id, second.id);
    provider.entered.notified().await;
    let own_result = tokio::time::timeout(Duration::from_secs(2), first.as_mut()).await;
    // Always release the second run, including when the regression times out.
    harness.cancel(&session.id);
    let second_finished = harness.wait_for_idle(&session.id).await?;
    assert_eq!(second_finished.id, second.id);
    assert_eq!(second_finished.status, "cancelled");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let result = own_result??;
    assert_eq!(result.content, "first result");
    assert!(!result.is_error);
    Ok(())
}
