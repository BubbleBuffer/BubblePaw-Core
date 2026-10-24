use super::*;

#[test]
fn deferred_metadata_survives_views_and_controls_native_promotion() -> Result<()> {
    let mut tools = ToolRegistry::new();
    tools.register_host(Arc::new(Capability::new("read")))?;
    tools.register_host(Arc::new(Capability::new("mcp__remote__read")))?;
    tools.mark_deferred("mcp__remote__read");
    tools.set_tool_server("mcp__remote__read", "Original server identity");
    for view in [
        tools.clone(),
        tools.restricted(&["js".into()]),
        tools.restricted_capabilities(&["mcp__remote__read".into()]),
        tools.read_only(),
        tools.with_execution_mode(ExecutionMode::Native),
    ] {
        assert!(view.is_deferred("mcp__remote__read"));
        assert_eq!(
            view.tool_server("mcp__remote__read"),
            Some("Original server identity")
        );
    }
    let native = tools.with_execution_mode(ExecutionMode::Native);
    assert!(native.get_model("read").is_some());
    assert!(native.get_model("mcp__remote__read").is_none());
    assert!(native.get_host("mcp__remote__read").is_some());
    assert!(native.get_model("tool_search").is_some());
    assert!(native.get_model("call_tool").is_some());
    tools.set_deferred("mcp__remote__read", false);
    assert!(
        tools
            .with_execution_mode(ExecutionMode::Native)
            .get_model("mcp__remote__read")
            .is_some()
    );
    tools.remove("mcp__remote__read", "disconnected");
    assert!(!tools.is_deferred("mcp__remote__read"));
    assert!(tools.tool_server("mcp__remote__read").is_none());
    Ok(())
}

fn route_arguments(route: &str, arguments: Value) -> Value {
    if route == "call_tool" {
        json!({"name":"read", "arguments":arguments})
    } else {
        arguments
    }
}

#[tokio::test]
async fn unified_dispatch_has_identical_validation_approval_and_allowlist_errors() -> Result<()> {
    for case in ["schema", "deny", "allowlist", "unavailable"] {
        let mut errors = Vec::new();
        for route in ["read", "js", "call_tool"] {
            let capability = Arc::new(Capability::new("read"));
            let store = Store::in_memory()?;
            let session = store.create_session("dispatch parity")?;
            let mut tools = ToolRegistry::new();
            tools.register_host(capability.clone())?;
            tools.register(Arc::new(JsBridge))?;
            if case == "unavailable" {
                tools.remove("read", "fixture disconnected");
            }
            let harness = Harness::new(
                store.clone(),
                Arc::new(RecordingProvider::default()),
                tools,
                HarnessConfig {
                    execution_mode: route_mode(route),
                    ..Default::default()
                },
            );
            let options = RunOptions {
                tool_names: (case == "allowlist").then(|| vec!["other".into()]),
                approval: (case == "deny").then(|| ApprovalPolicy {
                    deny: vec!["read".into()],
                    ..Default::default()
                }),
                ..Default::default()
            };
            let arguments = if case == "schema" {
                json!({"typo":"value"})
            } else {
                json!({"value":"hello"})
            };
            let result = harness
                .call_tool(
                    &session.id,
                    route,
                    route_arguments(route, arguments),
                    options,
                    CancellationToken::new(),
                )
                .await?;
            assert!(result.is_error, "{case}: {route}");
            errors.push(result.content);
            assert_eq!(capability.calls.load(Ordering::SeqCst), 0, "{case}");
            assert!(store.pending_effects(&session.id)?.is_empty());
        }
        assert_eq!(errors[0], errors[1], "{case}: native vs nested");
        assert_eq!(errors[0], errors[2], "{case}: native vs call_tool");
    }
    Ok(())
}

fn route_mode(route: &str) -> ExecutionMode {
    if route == "js" {
        ExecutionMode::Js
    } else {
        ExecutionMode::Native
    }
}

#[tokio::test]
async fn access_wrappers_allow_named_targets_and_enforce_the_shared_budget() -> Result<()> {
    for route in ["read", "js", "call_tool"] {
        let capability = Arc::new(Capability::new("read"));
        let (store, session, harness, _) = fixture(
            HarnessConfig {
                execution_mode: route_mode(route),
                ..Default::default()
            },
            capability.clone(),
        )?;
        let result = harness
            .call_tool(
                &session.id,
                route,
                route_arguments(route, json!({"value":"allowed"})),
                RunOptions {
                    tool_names: Some(vec!["read".into()]),
                    ..Default::default()
                },
                CancellationToken::new(),
            )
            .await?;
        assert!(!result.is_error, "{route}: {}", result.content);
        assert_eq!(result.content, "allowed");
        assert_eq!(capability.calls.load(Ordering::SeqCst), 1);
        if route != "read" {
            let result = harness
                .call_tool(
                    &session.id,
                    route,
                    route_arguments(route, json!({"value":"blocked"})),
                    RunOptions {
                        max_tool_calls: Some(1),
                        ..Default::default()
                    },
                    CancellationToken::new(),
                )
                .await?;
            assert!(result.is_error);
            assert!(
                result.content.contains("Tool call budget exhausted"),
                "{}",
                result.content
            );
            assert_eq!(capability.calls.load(Ordering::SeqCst), 1);
        }
        assert!(store.pending_effects(&session.id)?.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn model_and_embedder_routes_share_the_same_rejection_contract() -> Result<()> {
    struct OneCall(Mutex<Option<ToolCall>>);
    #[async_trait]
    impl Provider for OneCall {
        async fn generate(
            &self,
            _: ModelRequest,
            _: TextCallback,
            _: CancellationToken,
        ) -> Result<ModelResponse> {
            Ok(match self.0.lock().unwrap().take() {
                Some(call) => ModelResponse {
                    tool_calls: vec![call],
                    ..Default::default()
                },
                None => ModelResponse {
                    text: "done".into(),
                    ..Default::default()
                },
            })
        }
    }
    for case in ["schema", "deny", "allowlist"] {
        let mut expected = None;
        for route in ["read", "js", "call_tool"] {
            let store = Store::in_memory()?;
            let session = store.create_session("model and embedder dispatch parity")?;
            let capability = Arc::new(Capability::new("read"));
            let arguments = route_arguments(
                route,
                if case == "schema" {
                    json!({"unexpected":"argument"})
                } else {
                    json!({"value":"hello"})
                },
            );
            let provider = Arc::new(OneCall(Mutex::new(Some(ToolCall {
                id: "parity-call".into(),
                name: route.into(),
                arguments: arguments.clone(),
            }))));
            let mut tools = ToolRegistry::new();
            tools.register_host(capability.clone())?;
            tools.register(Arc::new(JsBridge))?;
            let harness = Harness::new(
                store.clone(),
                provider,
                tools,
                HarnessConfig {
                    execution_mode: route_mode(route),
                    ..Default::default()
                },
            );
            let options = RunOptions {
                tool_names: (case == "allowlist").then(|| vec!["other".into()]),
                approval: (case == "deny").then(|| ApprovalPolicy {
                    deny: vec!["read".into()],
                    ..Default::default()
                }),
                ..Default::default()
            };
            let run = harness
                .prompt(&session.id, "try the call", options.clone())
                .await?;
            assert_eq!(run.status, "completed", "{case}/{route}: {:?}", run.error);
            let model_error = store
                .messages(&session.id)?
                .into_iter()
                .find(|message| message.tool_call_id.as_deref() == Some("parity-call"))
                .expect("model rejection reply");
            assert!(model_error.is_error, "{case}/{route}");
            let embedder = harness
                .call_tool(
                    &session.id,
                    route,
                    arguments,
                    options,
                    CancellationToken::new(),
                )
                .await?;
            assert!(embedder.is_error);
            assert_eq!(model_error.content, embedder.content, "{case}/{route}");
            if let Some(expected) = &expected {
                assert_eq!(expected, &model_error.content, "{case}/{route}");
            } else {
                expected = Some(model_error.content);
            }
            assert_eq!(capability.calls.load(Ordering::SeqCst), 0);
        }
    }
    Ok(())
}
