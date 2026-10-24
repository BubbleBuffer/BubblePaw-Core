use super::*;

#[tokio::test]
async fn standard_profiles_keep_workspace_policy_and_reject_duplicate_names() {
    use bubblepaw_runner::{profiles::workspace_profiles, tools::register_native_tools};
    let root = tempfile::tempdir().unwrap();
    let store = Store::in_memory().unwrap();
    let repl = Arc::new(
        CodeMode::new(
            root.path().to_owned(),
            Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap()),
        )
        .unwrap(),
    );
    let provider = Arc::new(Probe::default());
    let mut tools = ToolRegistry::new();
    let processes = register_native_tools(&mut tools, root.path(), false).unwrap();
    let config = HarnessConfig {
        max_rounds: Some(30),
        ..Default::default()
    };
    let profiles = workspace_profiles(
        store.clone(),
        provider.clone(),
        &tools,
        &repl,
        &config,
        root.path(),
        true,
    )
    .unwrap();
    assert_eq!(
        profiles.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        ["read-only", "workspace"]
    );
    assert!(profiles.iter().all(|p| p.max_rounds == Some(30)));
    assert!(
        AgentTools::from_profiles(
            store.clone(),
            vec![profiles[0].clone(), profiles[0].clone()]
        )
        .is_err()
    );
    let mut zero = profiles[0].clone();
    zero.max_rounds = Some(0);
    assert!(AgentTools::from_profiles(store.clone(), vec![zero]).is_err());
    let limited = workspace_profiles(
        store.clone(),
        provider,
        &tools,
        &repl,
        &HarnessConfig {
            max_rounds: Some(3),
            ..config
        },
        root.path(),
        false,
    )
    .unwrap();
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].max_rounds, Some(3));
    assert!(
        research_profile_with_tools(
            store.clone(),
            Arc::new(Probe::default()),
            &tools,
            &repl,
            &HarnessConfig {
                max_rounds: Some(0),
                ..Default::default()
            },
            &WebConfig::default(),
        )
        .is_err()
    );
    let manager = Arc::new(AgentTools::from_profiles(store.clone(), limited).unwrap());
    manager.register(&mut tools).unwrap();
    assert!(
        profile_harness(
            store,
            Arc::new(Probe::default()),
            tools,
            &repl,
            HarnessConfig::default()
        )
        .is_err()
    );
    manager.shutdown().await.unwrap();
    processes.shutdown().await;
    repl.shutdown().await;
}
