use super::*;

#[tokio::test]
async fn mcp_is_deferred_by_default_and_defer_false_declares_native_without_restart() {
    let command = toml::Value::String(env!("CARGO_BIN_EXE_bubblepaw-runner-fake-mcp").into());
    let server = format!(
        "[mcp.servers.fixture]\ncommand={command}\napproval='none'\ninclude_tools=['echo','pid']\n"
    );
    let fixture = Fixture::new(&config(&server)).await;
    let context = || bubblepaw_core::ToolContext {
        session_id: fixture.session.clone(),
        run_id: "deferral-probe".into(),
        invocation_id: String::new(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    };
    let registry = fixture.live.registry();
    assert!(registry.is_deferred("mcp__fixture__echo"));
    assert_eq!(registry.tool_server("mcp__fixture__echo"), Some("fixture"));
    let declarations = registry
        .with_execution_mode(bubblepaw_core::ExecutionMode::Native)
        .definitions();
    assert!(
        !declarations
            .iter()
            .any(|tool| tool.name.starts_with("mcp__"))
    );
    assert!(!declarations.iter().any(|tool| tool.name == "js"));
    assert!(declarations.iter().any(|tool| tool.name == "read"));
    let pid = registry
        .get_host("mcp__fixture__pid")
        .unwrap()
        .invoke(context(), json!({}))
        .await
        .unwrap()
        .content;

    fixture.write(&config(&format!("{server}defer=false\n")));
    let report = fixture.live.reload(None).await;
    assert!(report.applied, "{:?}", report.errors);
    assert!(report.errors.is_empty());
    let registry = fixture.live.registry();
    assert!(!registry.is_deferred("mcp__fixture__echo"));
    assert_eq!(registry.tool_server("mcp__fixture__echo"), Some("fixture"));
    let declarations = registry
        .with_execution_mode(bubblepaw_core::ExecutionMode::Native)
        .definitions();
    assert!(
        declarations
            .iter()
            .any(|tool| tool.name == "mcp__fixture__echo")
    );
    assert!(
        declarations
            .iter()
            .any(|tool| tool.name == "mcp__fixture__pid")
    );
    assert_eq!(
        registry
            .get_host("mcp__fixture__pid")
            .unwrap()
            .invoke(context(), json!({}))
            .await
            .unwrap()
            .content,
        pid
    );
    assert!(
        report
            .changed
            .iter()
            .any(|change| change.contains("server fixture tool visibility settings updated"))
    );
    assert!(
        !report
            .changed
            .iter()
            .any(|change| change.contains("restarted"))
    );

    fixture.write(&config(&server));
    assert!(fixture.live.reload(None).await.applied);
    let registry = fixture.live.registry();
    assert!(registry.is_deferred("mcp__fixture__echo"));
    assert!(
        registry
            .with_execution_mode(bubblepaw_core::ExecutionMode::Native)
            .definitions()
            .iter()
            .all(|tool| !tool.name.starts_with("mcp__"))
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn mcp_filter_reload_updates_scripted_registry_and_notices_once() {
    let command = toml::Value::String(env!("CARGO_BIN_EXE_bubblepaw-runner-fake-mcp").into());
    let server = format!("[mcp.servers.fixture]\ncommand={command}\napproval='none'\n");
    let fixture = Fixture::new(&config(&format!("{server}include_tools=['echo','pid']\n"))).await;
    fixture.code(
        "initial",
        "return await tools.mcp__fixture__echo({text:'available'});",
    );
    fixture.turn("initial").await;
    assert!(fixture.live.registry().get("mcp__fixture__echo").is_some());
    assert!(fixture.live.registry().get("mcp__fixture__fail").is_none());
    fixture.write(&config(&format!(
        "{server}include_tools=['*']\nexclude_tools=['echo']\n"
    )));
    fixture.code("filtered", "const names=(await tools.$list()).map(t=>t.name); if(names.includes('mcp__fixture__echo')) throw Error('filtered tool advertised'); if(!names.includes('mcp__fixture__fail')) throw Error('cached tool not restored'); return 'updated';");
    fixture.turn("filtered").await;
    assert!(fixture.live.registry().get("mcp__fixture__echo").is_none());
    assert!(fixture.live.registry().get("mcp__fixture__fail").is_some());
    assert_eq!(fixture.notices().len(), 1);
    assert!(fixture.notices()[0].contains("server fixture tool visibility settings updated"));
    assert!(fixture.notices()[0].contains("− mcp__fixture__echo"));
    fixture.turn("unchanged").await;
    assert_eq!(fixture.notices().len(), 1);
    fixture.write(&config(&format!("{server}include_tools=[]\n")));
    fixture.turn("explicit empty").await;
    assert!(fixture.mcp.tools().is_empty());
    assert_eq!(fixture.notices().len(), 2);
    fixture.shutdown().await;
}
