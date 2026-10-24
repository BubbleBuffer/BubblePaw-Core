use super::*;

#[tokio::test]
async fn single_tool_call_validates_names_arguments_and_entrypoint_restrictions() -> Result<()> {
    let capability = Arc::new(Capability::new("read"));
    let (store, session, harness, provider) = fixture(native_config(), capability.clone())?;
    for (name, input, options, expected) in [
        (
            "missing",
            json!({}),
            RunOptions::default(),
            "Unknown tool: missing",
        ),
        (
            "read",
            json!({"typo":"hello"}),
            RunOptions::default(),
            "Invalid arguments",
        ),
        (
            "read",
            json!({"value":"hello"}),
            RunOptions {
                tool_names: Some(vec![]),
                ..Default::default()
            },
            "Allowed tools: none.",
        ),
    ] {
        let result = harness
            .call_tool(&session.id, name, input, options, CancellationToken::new())
            .await?;
        assert!(result.is_error);
        assert!(result.content.contains(expected), "{}", result.content);
        assert!(store.pending_effects(&session.id)?.is_empty());
        assert_eq!(harness.wait_for_idle(&session.id).await?.status, "failed");
    }
    assert_eq!(capability.calls.load(Ordering::SeqCst), 0);
    assert!(provider.requests.lock().unwrap().is_empty());
    let (_, session, harness, _) = fixture(
        HarnessConfig {
            execution_mode: ExecutionMode::Js,
            ..Default::default()
        },
        Arc::new(Capability::new("read")),
    )?;
    assert!(
        !harness
            .call_tool(
                &session.id,
                "read",
                json!({"value":"hello"}),
                RunOptions::default(),
                CancellationToken::new()
            )
            .await?
            .is_error
    );
    assert!(
        !harness
            .call_tool(
                &session.id,
                "js",
                json!({"value":"hello"}),
                RunOptions::default(),
                CancellationToken::new()
            )
            .await?
            .is_error
    );
    Ok(())
}

#[tokio::test]
async fn single_tool_call_refuses_interactive_approval_direct_and_nested_but_honors_allow()
-> Result<()> {
    for (mode, route) in [(ExecutionMode::Native, "read"), (ExecutionMode::Js, "js")] {
        let capability = Arc::new(Capability {
            approval: Approval::Confirm,
            ..Capability::new("read")
        });
        let (store, session, harness, provider) = fixture(
            HarnessConfig {
                execution_mode: mode,
                ..Default::default()
            },
            capability.clone(),
        )?;
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            harness.call_tool(
                &session.id,
                route,
                json!({"value":"hello"}),
                RunOptions::default(),
                CancellationToken::new(),
            ),
        )
        .await??;
        assert!(result.is_error);
        assert!(
            result
                .content
                .contains("Interactive approval is unavailable"),
            "{}",
            result.content
        );
        assert!(harness.pending_approvals(&session.id)?.is_empty());
        assert!(store.pending_effects(&session.id)?.is_empty());
        assert_eq!(capability.calls.load(Ordering::SeqCst), 0);
        let options = RunOptions {
            approval: Some(ApprovalPolicy {
                confirm: ApprovalMode::Allow,
                ..Default::default()
            }),
            ..Default::default()
        };
        let allowed = harness
            .call_tool(
                &session.id,
                route,
                json!({"value":"allowed"}),
                options,
                CancellationToken::new(),
            )
            .await?;
        assert!(!allowed.is_error, "{}", allowed.content);
        assert_eq!(capability.calls.load(Ordering::SeqCst), 1);
        assert!(provider.requests.lock().unwrap().is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn single_tool_call_cancellation_ownership_and_queued_input() -> Result<()> {
    let entered = Arc::new(Notify::new());
    let capability = Arc::new(Capability {
        blocked: Some(entered.clone()),
        ..Capability::new("read")
    });
    let (store, session, harness, provider) = fixture(native_config(), capability)?;
    assert!(
        harness
            .call_tool(
                "missing-session",
                "read",
                json!({"value":"hello"}),
                RunOptions::default(),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(
        harness
            .call_tool(
                &session.id,
                "read",
                json!({"value":"hello"}),
                RunOptions::default(),
                cancelled
            )
            .await
            .is_err()
    );
    assert!(store.messages(&session.id)?.is_empty());
    let cancellation = CancellationToken::new();
    let task = {
        let harness = harness.clone();
        let session_id = session.id.clone();
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            harness
                .call_tool(
                    &session_id,
                    "read",
                    json!({"value":"hello"}),
                    RunOptions::default(),
                    cancellation,
                )
                .await
        })
    };
    entered.notified().await;
    let busy = harness
        .call_tool(
            &session.id,
            "read",
            json!({"value":"hello"}),
            RunOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(busy.downcast_ref::<SessionBusy>().is_some());
    harness.followup(&session.id, "queued")?;
    cancellation.cancel();
    assert!(task.await?.is_err());
    assert_eq!(
        harness.wait_for_idle(&session.id).await?.status,
        "cancelled"
    );
    assert!(
        store
            .queued_inputs(&session.id)?
            .iter()
            .any(|input| input.text == "queued" && input.state == "pending")
    );
    assert!(provider.requests.lock().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn single_tool_call_rejects_read_only_sessions_and_ambiguous_script_options() -> Result<()> {
    let (store, mut session, harness, provider) = fixture(
        native_config(),
        Arc::new(Capability {
            read_only: false,
            ..Capability::new("write")
        }),
    )?;
    let options = RunOptions {
        script: Some("return 1".into()),
        ..Default::default()
    };
    let error = harness
        .call_tool(
            &session.id,
            "write",
            json!({"value":"hello"}),
            options,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cannot be combined"));
    session.read_only = true;
    store.write(|tx| {
        tx.execute(
            "UPDATE sessions SET data=?2 WHERE id=?1",
            rusqlite::params![session.id, serde_json::to_string(&session)?],
        )?;
        Ok(())
    })?;
    assert_eq!(
        names(harness.effective_tool_definitions(&session.id, &RunOptions::default())?),
        ["call_tool", "tool_search"]
    );
    let error = harness
        .call_tool(
            &session.id,
            "write",
            json!({"value":"hello"}),
            RunOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("read only"));
    assert!(store.messages(&session.id)?.is_empty());
    assert!(provider.requests.lock().unwrap().is_empty());
    Ok(())
}
