use super::*;

pub(super) async fn run() -> Result<()> {
    use bubblepaw_core::EventKind;
    use bubblepaw_runner::{
        agents::AgentRegistry, config::Agents, profiles::workspace_profiles_with_agents,
    };
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let session = store.create_session("nested parent")?;
    let provider = Arc::new(CellProvider);
    let registry = Arc::new(AgentRegistry::default());
    let limits = Agents {
        max_depth: Some(2),
        ..Default::default()
    };
    let config = HarnessConfig {
        execution_mode: bubblepaw_core::ExecutionMode::Js,
        ..Default::default()
    };
    let repl = Arc::new(CodeMode::new(
        directory.path().to_owned(),
        Arc::new(ArtifactStore::new(
            directory.path(),
            directory.path().join("artifacts"),
        )?),
    )?);
    let mut tools = ToolRegistry::new();
    let processes = register_native_tools(&mut tools, directory.path(), true)?;
    let leaf_profiles = workspace_profiles_with_agents(
        store.clone(),
        provider.clone(),
        &tools,
        &repl,
        &config,
        directory.path(),
        false,
        None,
    )?;
    let nested = Arc::new(
        AgentTools::from_profiles(store.clone(), leaf_profiles)?
            .with_registry(registry.clone())
            .with_limits(limits.clone())
            .with_repl(repl.clone()),
    );
    let root_profiles = workspace_profiles_with_agents(
        store.clone(),
        provider.clone(),
        &tools,
        &repl,
        &config,
        directory.path(),
        false,
        Some(&nested),
    )?;
    let agents = Arc::new(
        AgentTools::from_profiles(store.clone(), root_profiles)?
            .with_registry(registry)
            .with_limits(limits)
            .with_repl(repl.clone())
            .with_nested(vec![nested]),
    );
    let mut parent_tools = ToolRegistry::new();
    agents.register(&mut parent_tools)?;
    register_agent_handles(&mut parent_tools, agents.clone())?;
    repl.register(&mut parent_tools)?;
    let parent = Harness::new(store.clone(), provider, parent_tools, config);
    let leaf_task = "CELL:if('agent' in tools) throw Error('depth capability leak'); text('leaf ready'); return await wait({ms:60000});";
    let child_task = format!(
        "CELL:const leaf=await tools.agent({}); return await wait({{sessionId:leaf.sessionId}});",
        json!({"task":leaf_task})
    );
    let script = format!(
        "const child=await tools.agent({}); return await wait({{sessionId:child.sessionId}});",
        json!({"task":child_task})
    );
    let mut events = parent.subscribe();
    parent.start(
        &session.id,
        "nested probe",
        RunOptions {
            script: Some(script),
            ..Default::default()
        },
    )?;
    let leaf_id = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let event = events.recv().await?;
            if let EventKind::Output { record } = event.kind
                && record.output["text"] == "leaf ready"
            {
                return Ok::<String, anyhow::Error>(event.session_id);
            }
        }
    })
    .await??;
    let child_id = store.session(&leaf_id)?.parent_session_id.unwrap();
    assert_eq!(
        store.session(&child_id)?.parent_session_id.as_deref(),
        Some(session.id.as_str())
    );
    // The output is emitted before wait is dispatched. Establish the sleep
    // before cancellation so a missing wait capability cannot race to a final
    // model response and make this a test of scheduling instead of cleanup.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            anyhow::ensure!(
                store.runs(&leaf_id)?[0].status == "running",
                "leaf stopped before entering its wait"
            );
            if repl.status(&leaf_id)["environments"]
                .as_array()
                .unwrap()
                .iter()
                .any(|environment| environment["time_waiting"] == 1)
            {
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await??;
    assert!(parent.cancel(&session.id));
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        parent.wait_for_idle(&session.id),
    )
    .await??;
    // Await the existing observer, rather than shutdown causing the cleanup.
    let child = agents.wait(&session.id, &child_id, Some(5000)).await?;
    assert_eq!(child["status"], "cancelled");
    for id in [&child_id, &leaf_id] {
        assert_eq!(store.runs(id)?[0].status, "cancelled");
        assert!(store.runs(id)?[0].ended_at.is_some());
        assert!(
            repl.status(id)["environments"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    agents.shutdown().await?;
    repl.shutdown().await;
    processes.shutdown().await;
    Ok(())
}
