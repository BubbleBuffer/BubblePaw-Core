use super::*;

#[tokio::test]
async fn explicit_workspace_access_allows_writes_and_cleanup_awaits_owned_commands() {
    let fixture = Fixture::new();
    let mut registry = ToolRegistry::new();
    let processes = register_native_tools(&mut registry, fixture.root.path(), false).unwrap();
    fixture.repl.register(&mut registry).unwrap();
    let read_only = Harness::new(
        fixture.store.clone(),
        fixture.provider.clone(),
        registry.read_only(),
        HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            ..Default::default()
        },
    );
    let writable = Harness::new(
        fixture.store.clone(),
        fixture.provider.clone(),
        registry,
        HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            ..Default::default()
        },
    );
    let manager = AgentTools::new(fixture.store.clone(), read_only, Some(writable))
        .with_repl(fixture.repl.clone())
        .with_processes(processes.clone());
    let denied = manager
        .start(&fixture.owner, json!({"task":"workspace child"}))
        .unwrap();
    let result = manager
        .wait(
            &fixture.owner.session_id,
            denied["handle"].as_str().unwrap(),
            Some(2000),
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "failed");
    assert!(!fixture.root.path().join("written.txt").exists());
    let granted = manager
        .start(
            &fixture.owner,
            json!({"task":"workspace child","access":"workspace"}),
        )
        .unwrap();
    let result = manager
        .wait(
            &fixture.owner.session_id,
            granted["handle"].as_str().unwrap(),
            Some(15_000),
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(
        std::fs::read_to_string(fixture.root.path().join("written.txt")).unwrap(),
        "explicit workspace child\n"
    );
    let child_id = granted["sessionId"].as_str().unwrap();
    assert!(
        fixture
            .store
            .outputs(child_id, 10)
            .unwrap()
            .iter()
            .any(|output| output.output["text"]
                .as_str()
                .is_some_and(|text| text.starts_with("process:")))
    );
    // The process completion observer is independent of the child's final answer.
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !fixture.store.pending_effects(child_id).unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        fixture.repl.status(child_id)["environments"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    manager.shutdown().await.unwrap();
    processes.shutdown().await;
    fixture.shutdown().await;
}
