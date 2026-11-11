use super::*;

#[tokio::test]
async fn history_capabilities_work_through_js_in_read_only_mode() {
    let fixture = Fixture::new();
    fixture.message("ArchivedExactDetail");
    assert!(fixture.registry.definitions().is_empty());
    assert_eq!(fixture.registry.read_only().capabilities().len(), 3);
    assert!(
        fixture
            .registry
            .capabilities()
            .iter()
            .all(|definition| definition.read_only)
    );
    let repl = Arc::new(
        CodeMode::new(
            fixture.root.path().to_owned(),
            Arc::new(
                ArtifactStore::new(fixture.root.path(), fixture.root.path().join("artifacts"))
                    .unwrap(),
            ),
        )
        .unwrap(),
    );
    let mut registry = fixture.registry.read_only();
    repl.register(&mut registry).unwrap();
    let provider = Arc::new(ScriptedProvider::with_steps(
        vec![ScriptedStep::Calls {
            calls: vec![ToolCall {
                id: "js-call".into(),
                name: "js".into(),
                arguments: json!({"code":"const chapters=await tools.history_chapters({}); const found=await tools.history_search({query:'archivedexactdetail'}); const page=await tools.history_read({from:found.matches[0].index,count:1}); if(page.messages[0].text!=='ArchivedExactDetail') throw Error('lost history'); text(JSON.stringify({found:found.matches[0].index,chapters:chapters.chapters}));"}),
            }],
        }],
        "finished",
    ));
    let harness = Harness::new(
        fixture.store.clone(),
        provider,
        registry,
        HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            ..Default::default()
        },
    );
    assert_eq!(
        harness
            .prompt(
                &fixture.session,
                "Find archived detail",
                RunOptions::default()
            )
            .await
            .unwrap()
            .status,
        "completed"
    );
    let outputs = fixture.store.outputs(&fixture.session, 10).unwrap();
    let text = outputs
        .iter()
        .find_map(|output| output.output["text"].as_str())
        .expect("JS did not publish archive result");
    assert_eq!(
        serde_json::from_str::<Value>(text).unwrap(),
        json!({"found":0,"chapters":[]})
    );
    repl.shutdown().await;
}
