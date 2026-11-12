use super::*;

#[tokio::test]
async fn chat_prefix_survives_store_reopen_while_live_calls_and_new_chats_use_current_tools()
-> Result<()> {
    for mode in [ExecutionMode::Native, ExecutionMode::Js] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("store.db");
        let f = fixture_in_store(
            mode,
            true,
            vec![
                done(),
                routed(mode, "new_core", json!({"value":"reopened live tool"})),
                done(),
                done(),
            ],
            false,
            true,
            Store::open(&path)?,
        )?;
        let first = f
            .harness
            .prompt(&f.session, "initial turn", RunOptions::default())
            .await?;
        assert_eq!(first.status, "completed", "{:?}", first.error);
        let saved = frozen(&f.provider.requests.lock().unwrap()[0])?;
        let session = f.session.clone();
        let inventory = f.inventory.clone();
        let provider = f.provider.clone();
        publish_inventory(&mut inventory.write().unwrap())?;
        f.harness
            .shutdown(std::time::Duration::from_secs(5))
            .await?;
        drop(f);

        let store = Store::open(&path)?;
        let resolver = inventory.clone();
        let harness = Harness::new(
            store.clone(),
            provider.clone(),
            ToolRegistry::new(),
            HarnessConfig {
                execution_mode: mode,
                system_prompt: "Changed configured instructions after restart".into(),
                registry_resolver: Some(Arc::new(move || resolver.read().unwrap().clone())),
                ..Default::default()
            },
        );
        assert_eq!(frozen(&harness.request_template(&session)?)?, saved);
        let resumed = harness
            .prompt(&session, "resume", RunOptions::default())
            .await?;
        assert_eq!(resumed.status, "completed", "{:?}", resumed.error);
        assert!(
            store
                .messages(&session)?
                .iter()
                .any(|message| message.role == Role::Tool
                    && !message.is_error
                    && message.content.contains("reopened live tool"))
        );

        let fresh = store.create_session("new chat after restart")?.id;
        let run = harness
            .prompt(&fresh, "fresh", RunOptions::default())
            .await?;
        assert_eq!(run.status, "completed", "{:?}", run.error);
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        for request in &requests[1..3] {
            assert_eq!(frozen(request)?, saved);
        }
        let fresh_request = &requests[3];
        assert!(
            fresh_request
                .system_prompt
                .starts_with("Changed configured instructions after restart")
        );
        let declarations = serde_json::to_string(&fresh_request.tools)?;
        assert!(declarations.contains("new_core"));
        assert!(!declarations.contains("former_core"));
        assert_ne!(frozen(fresh_request)?, saved);
        if mode == ExecutionMode::Native {
            assert!(
                fresh_request
                    .tools
                    .iter()
                    .any(|tool| tool.name == "new_core")
            );
            assert!(fresh_request.system_prompt.contains("late.server"));
            assert!(!fresh_request.system_prompt.contains("original.server"));
        }
    }
    Ok(())
}

#[tokio::test]
async fn a_changed_run_allowlist_remains_live_without_rewriting_the_chat_declarations() -> Result<()>
{
    for mode in [ExecutionMode::Native, ExecutionMode::Js] {
        let f = fixture(
            mode,
            true,
            vec![
                routed(mode, "former_core", json!({"value":"initially allowed"})),
                done(),
                routed(mode, "former_core", json!({"value":"must be denied"})),
                routed(
                    mode,
                    "mcp__old__lookup",
                    json!({"value":"currently allowed"}),
                ),
                done(),
            ],
            false,
            true,
        )?;
        for allowed in ["former_core", "mcp__old__lookup"] {
            let run = f
                .harness
                .prompt(
                    &f.session,
                    "work",
                    RunOptions {
                        tool_names: Some(vec![allowed.into()]),
                        ..Default::default()
                    },
                )
                .await?;
            assert_eq!(run.status, "completed", "{:?}", run.error);
        }
        let requests = f.provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 5);
        let original = frozen(&requests[0])?;
        for request in &requests[1..] {
            assert_eq!(frozen(request)?, original);
        }
        let history = f.store.messages(&f.session)?;
        assert!(history.iter().any(|message| message.role == Role::Tool
            && message.is_error
            && message.content.contains("not available in this run")));
        assert!(history.iter().any(|message| message.role == Role::Tool
            && !message.is_error
            && message.content.contains("currently allowed")));
        assert!(!history.iter().any(|message| message.role == Role::Tool
            && !message.is_error
            && message.content.contains("must be denied")));
    }
    Ok(())
}
