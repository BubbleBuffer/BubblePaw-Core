use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_awaits_descendant_cleanup_and_all_spawn_effects_settle() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let parent = store.create_session("parent")?;
    let (entered_tx, mut entered) = mpsc::unbounded_channel();
    let dropped = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(NestedProbe {
        entered: entered_tx,
        dropped: dropped.clone(),
    });
    let repl = Arc::new(CodeMode::new(
        directory.path().to_owned(),
        Arc::new(ArtifactStore::new(
            directory.path(),
            directory.path().join("artifacts"),
        )?),
    )?);
    let manager = AgentTools::new_recursive(
        store.clone(),
        Agents::default(),
        Arc::new(AgentRegistry::default()),
        Some(repl.clone()),
        None,
        |manager| {
            let mut tools = ToolRegistry::new();
            manager.register_weak(&mut tools)?;
            register_agent_handles_weak(&mut tools, manager)?;
            repl.register(&mut tools)?;
            let harness = Arc::new(Harness::new(
                store.clone(),
                provider.clone(),
                tools,
                HarnessConfig {
                    execution_mode: bubblepaw_core::ExecutionMode::Js,
                    ..Default::default()
                },
            ));
            Ok(vec![AgentProfile {
                name: "read-only".into(),
                description: "nested timeout fixture".into(),
                harness,
                model: None,
                max_rounds: Some(12),
                approval: None,
            }])
        },
    )?;
    let owner = ToolContext {
        session_id: parent.id,
        run_id: "parent-run".into(),
        invocation_id: "spawn".into(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    };
    let branch = manager.start(&owner, json!({"task":"branch","timeout":1500}))?;
    let grandchild = tokio::time::timeout(Duration::from_secs(3), entered.recv())
        .await?
        .expect("grandchild provider started");
    let result = manager
        .wait(
            &owner.session_id,
            branch["sessionId"].as_str().unwrap(),
            Some(5000),
        )
        .await?;
    assert_eq!(result["status"], "timed_out", "{result}");
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert_eq!(store.runs(&grandchild)?[0].status, "cancelled");
    for session in store.list_sessions()? {
        let effects = store.pending_effects(&session.id)?;
        let connection = rusqlite::Connection::open_with_flags(
            store.chat_path(&session.id)?.expect("chat database"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let messages = store.messages(&session.id)?;
        let diagnostics = effects
            .iter()
            .map(|effect| -> Result<Value> {
                let call_id: String = connection.query_row(
                    "SELECT call_id FROM effects WHERE id=?1 AND session_id=?2",
                    rusqlite::params![effect["id"].as_str().unwrap(), session.id],
                    |row| row.get(0),
                )?;
                // Outer effects intentionally have no detail. Resolve their exact
                // call_id through canonical assistant tool calls instead.
                let name = effect["call"]["name"].as_str().or_else(|| {
                    messages
                        .iter()
                        .flat_map(|message| &message.tool_calls)
                        .find(|call| call.id == call_id)
                        .map(|call| call.name.as_str())
                });
                Ok(json!({"id":effect["id"],"callId":call_id,"name":name,"state":effect["state"]}))
            })
            .collect::<Result<Vec<_>>>()?;
        // Cancellation of an in-flight JS wrapper may leave an unknown
        // historical outcome. Its spawned child is known cancelled and its
        // agent effect must settle; no dispatched/running effect may remain.
        assert!(
            diagnostics
                .iter()
                .all(|effect| effect["state"] == "unknown" && effect["name"] == "js"),
            "unexpected pending effect in {}: {}",
            session.id,
            serde_json::to_string(&diagnostics)?
        );
        assert!(
            repl.status(&session.id)["environments"]
                .as_array()
                .unwrap()
                .is_empty(),
            "open JS in {}",
            session.id
        );
    }
    manager.shutdown().await?;
    repl.shutdown().await;
    Ok(())
}
