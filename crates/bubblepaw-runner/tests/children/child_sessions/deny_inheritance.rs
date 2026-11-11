use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recursive_deny_rules_union_at_every_generation_with_pinned_override() -> Result<()> {
    for profile_additions in [true, false] {
        let directory = tempfile::tempdir()?;
        let store = Store::open_data_dir(directory.path())?;
        let root = store.create_session("recursive deny root")?;
        let (entered_tx, mut entered) = mpsc::unbounded_channel();
        let provider = Arc::new(RecursiveDenyProvider {
            entered: entered_tx,
            profile_additions,
        });
        let effects = Arc::new(RecursiveDenyEffects::default());
        let repl = Arc::new(CodeMode::new(
            directory.path().to_owned(),
            Arc::new(ArtifactStore::new(
                directory.path(),
                directory.path().join("artifacts"),
            )?),
        )?);
        let mut base = ToolRegistry::new();
        let processes = register_native_tools(&mut base, directory.path(), false)?;
        base.register(Arc::new(Bridge))?;
        for name in RECURSIVE_DENY_TOOLS {
            base.register(Arc::new(RecursiveDenyEffect {
                name,
                effects: effects.clone(),
            }))?;
        }
        let config = HarnessConfig {
            approval: ApprovalPolicy {
                deny: vec!["root_effect".into()],
                ..allow()
            },
            ..Default::default()
        };
        let limits = Agents {
            auto_wake: false,
            ..Default::default()
        };
        let manager = if profile_additions {
            // The same recursive manager builds the real nested host tools.
            // Profile harness defaults intentionally omit the root denies, so
            // only inheritance through actual dispatch can preserve them.
            AgentTools::new_recursive(
                store.clone(),
                limits,
                Arc::new(AgentRegistry::default()),
                Some(repl.clone()),
                Some(processes.clone()),
                |manager| {
                    [
                        ("workspace", "child_effect"),
                        ("restricted", "grandchild_effect"),
                    ]
                    .into_iter()
                    .map(|(name, deny)| {
                        let mut tools = base.clone();
                        manager.register_weak(&mut tools)?;
                        Ok(AgentProfile {
                            name: name.into(),
                            description: "Recursive approval scope fixture".into(),
                            model: None,
                            max_rounds: Some(12),
                            approval: Some(ApprovalPolicy {
                                deny: vec![deny.into()],
                                ..allow()
                            }),
                            harness: Arc::new(Harness::new(
                                store.clone(),
                                provider.clone(),
                                tools,
                                HarnessConfig {
                                    approval: allow(),
                                    ..Default::default()
                                },
                            )),
                        })
                    })
                    .collect()
                },
            )?
        } else {
            // Also exercise the production host's live approval-source chain
            // through three nested workspace children, with a pinned override.
            child_agent_host(
                store.clone(),
                provider.clone(),
                &base,
                &repl,
                &config,
                directory.path(),
                true,
                None,
                limits,
                Arc::new(AgentRegistry::default()),
                processes.clone(),
            )?
        };
        let mut root_tools = base;
        manager.register(&mut root_tools)?;
        let parent = Harness::new(store.clone(), provider, root_tools, config);
        let mut events = parent.subscribe();
        let run = tokio::time::timeout(
            DEADLINE,
            parent.prompt(
                &root.id,
                "recursive-deny:0",
                RunOptions {
                    approval: Some(ApprovalPolicy {
                        deny: vec!["override_effect".into()],
                        ..allow()
                    }),
                    ..Default::default()
                },
            ),
        )
        .await??;
        assert_eq!(run.status, "completed");
        let mut nodes = vec![None; 4];
        for _ in 0..4 {
            let (depth, session) = tokio::time::timeout(DEADLINE, entered.recv())
                .await?
                .unwrap();
            assert!(
                nodes[depth].replace(session).is_none(),
                "one node per depth"
            );
        }
        let nodes = nodes.into_iter().map(Option::unwrap).collect::<Vec<_>>();
        assert_eq!(nodes[0], root.id);
        for depth in 1..4 {
            assert_eq!(
                store.session(&nodes[depth])?.parent_session_id.as_deref(),
                Some(nodes[depth - 1].as_str())
            );
            let result = manager
                .wait(&nodes[depth - 1], &nodes[depth], Some(5000))
                .await?;
            assert_eq!(result["status"], "completed", "depth {depth}: {result}");
        }
        let observed_events = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
        for (depth, session) in nodes.iter().enumerate() {
            let expected_denies = RECURSIVE_DENY_TOOLS
                .iter()
                .copied()
                .filter(|tool| {
                    matches!(*tool, "root_effect" | "override_effect")
                        || (profile_additions && depth >= 1 && *tool == "child_effect")
                        || (profile_additions && depth >= 2 && *tool == "grandchild_effect")
                })
                .collect::<std::collections::BTreeSet<_>>();
            let policy = effects.policies.lock().unwrap()[session].clone();
            assert_eq!(
                policy
                    .deny
                    .iter()
                    .map(String::as_str)
                    .collect::<std::collections::BTreeSet<_>>(),
                expected_denies,
                "effective deny union at depth {depth}, custom profiles={profile_additions}"
            );
            let messages = store.messages(session)?;
            for tool in RECURSIVE_DENY_TOOLS {
                let denied = expected_denies.contains(tool);
                let count = effects
                    .calls
                    .lock()
                    .unwrap()
                    .get(&(session.clone(), tool.into()))
                    .copied()
                    .unwrap_or_default();
                assert_eq!(
                    count,
                    usize::from(!denied),
                    "{tool} dispatches at depth {depth}"
                );
                let call_id = format!("recursive-deny-{depth}-{tool}");
                let reply = messages
                    .iter()
                    .find(|message| message.tool_call_id.as_deref() == Some(call_id.as_str()))
                    .unwrap();
                assert_eq!(
                    reply.is_error, denied,
                    "{tool} result at depth {depth}: {}",
                    reply.content
                );
                if denied {
                    assert!(
                        reply.content.contains(&format!("deny rule {tool}")),
                        "{}",
                        reply.content
                    );
                    assert!(!observed_events.iter().any(|event| event.session_id == *session
                        && matches!(&event.kind, EventKind::ToolStarted { call } if call.name == tool)));
                    assert!(observed_events.iter().any(|event| event.session_id == *session
                        && matches!(&event.kind, EventKind::ApprovalRequested { approval }
                            if approval.tool == tool && approval.state == bubblepaw_core::ApprovalState::Denied
                                && approval.reason.as_deref() == Some(format!("deny rule {tool}").as_str()))));
                }
            }
            assert!(
                store.pending_effects(session)?.is_empty(),
                "all admitted effects settled at depth {depth}"
            );
        }
        manager.shutdown().await?;
        processes.shutdown().await;
        repl.shutdown().await;
    }
    Ok(())
}
