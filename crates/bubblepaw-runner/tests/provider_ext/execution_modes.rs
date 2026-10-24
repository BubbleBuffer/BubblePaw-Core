use super::*;

#[tokio::test]
async fn extension_wire_tools_match_harness_introspection_in_every_execution_mode() {
    for execution_mode in [ExecutionMode::Js, ExecutionMode::Native] {
        let root = tempfile::tempdir().unwrap();
        let store = Store::in_memory().unwrap();
        let session = store.create_session("extension tool surface").unwrap().id;
        let repl = Arc::new(
            CodeMode::new(
                root.path().to_owned(),
                Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap()),
            )
            .unwrap(),
        );
        let mut tools = ToolRegistry::new();
        tools.register_host(Arc::new(Named("echo"))).unwrap();
        repl.register(&mut tools).unwrap();
        let provider = Arc::new(fake());
        provider.start().await.unwrap();
        let harness = Harness::new(
            store.clone(),
            provider,
            tools,
            HarnessConfig {
                model: "code-model".into(),
                execution_mode,
                ..Default::default()
            },
        );
        let expected = harness
            .effective_tool_definitions(&session, &RunOptions::default())
            .unwrap()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert_eq!(
            expected.iter().any(|name| name == "echo"),
            execution_mode == ExecutionMode::Native
        );
        assert_eq!(
            expected.iter().any(|name| name == "js"),
            execution_mode == ExecutionMode::Js
        );
        let run = harness
            .prompt(&session, "text ready", RunOptions::default())
            .await
            .unwrap();
        assert_eq!(run.status, "completed", "{:?}", run.error);
        let messages = store.messages(&session).unwrap();
        let state = messages
            .iter()
            .find(|message| message.role == Role::Assistant)
            .unwrap()
            .provider_state
            .as_ref()
            .unwrap();
        assert_eq!(state["tools"], json!(expected));
        repl.shutdown().await;
    }
}
