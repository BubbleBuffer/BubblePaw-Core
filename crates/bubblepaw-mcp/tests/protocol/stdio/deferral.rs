use super::*;
use bubblepaw_core::{ExecutionMode, ToolRegistry};

#[tokio::test]
async fn publishes_server_identity_and_updates_deferral_without_reconnecting() {
    let root = tempfile::tempdir().unwrap();
    // The configured identity is deliberately longer than a registered tool
    // name, so discovery must use metadata rather than reverse parsing it.
    let server = "long_server_identity_that_cannot_fit_inside_a_single_registered_mcp_tool_name";
    let mut server_config = config(Vec::new());
    server_config.include_tools = Some(vec!["echo".into(), "pid".into()]);
    let mut settings = McpSettings {
        servers: BTreeMap::from([(server.into(), server_config)]),
        ..Default::default()
    };
    let host = McpHost::start(&settings, root.path()).await;
    assert!(host.warnings().is_empty(), "{:?}", host.warnings());
    let mut registry = ToolRegistry::new();
    host.register_tools(&mut registry).unwrap();
    let definitions = registry.capabilities();
    assert_eq!(definitions.len(), 2);
    for definition in &definitions {
        assert!(registry.is_deferred(&definition.name));
        assert_eq!(registry.tool_server(&definition.name), Some(server));
        assert!(definition.name.len() <= 64);
    }
    assert!(
        registry
            .with_execution_mode(ExecutionMode::Native)
            .definitions()
            .iter()
            .all(|tool| !tool.name.starts_with("mcp__"))
    );
    let pid_name = bubblepaw_mcp::tool_name(server, "pid");
    let pid_tool = registry.get_host(&pid_name).unwrap();
    let pid = pid_tool.invoke(context(), json!({})).await.unwrap().content;

    settings.servers.get_mut(server).unwrap().defer = false;
    let diff = host.reload(&settings, None).await.unwrap();
    assert!(diff.restarted.is_empty());
    assert!(diff.started.is_empty());
    assert_eq!(diff.filtered, [server]);
    let mut registry = ToolRegistry::new();
    host.register_tools(&mut registry).unwrap();
    for definition in &definitions {
        assert!(!registry.is_deferred(&definition.name));
        assert_eq!(registry.tool_server(&definition.name), Some(server));
    }
    assert_eq!(
        registry
            .with_execution_mode(ExecutionMode::Native)
            .definitions()
            .iter()
            .filter(|tool| tool.name.starts_with("mcp__"))
            .count(),
        2
    );
    assert_eq!(
        pid_tool.invoke(context(), json!({})).await.unwrap().content,
        pid
    );
    host.shutdown(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn discovers_pages_preserves_approval_and_converts_results() {
    let root = tempfile::tempdir().unwrap();
    let log = root.path().join("requests.jsonl");
    let mut settings = config(vec![
        "--log".into(),
        log.display().to_string(),
        "--paginate".into(),
    ]);
    settings.tools.insert(
        "echo".into(),
        McpToolSettings {
            approval: Some(Approval::None),
        },
    );
    let host = start(settings, root.path()).await;
    assert!(host.warnings().is_empty(), "{:?}", host.warnings());
    assert_eq!(host.tools().len(), 13);
    for tool in host.tools() {
        let definition = tool.definition();
        assert!(!definition.read_only);
        assert_eq!(
            definition.approval,
            if definition.name.ends_with("__echo") {
                Approval::None
            } else {
                Approval::Confirm
            }
        );
    }
    assert_eq!(
        invoke(&host, "echo", json!({"text":"hello"})).await.content,
        "hello"
    );
    assert!(invoke(&host, "fail", json!({})).await.is_error);
    let rpc = invoke(&host, "rpc_error", json!({})).await;
    assert!(rpc.is_error);
    assert!(rpc.content.contains("fake RPC failure"));
    assert_eq!(
        invoke(&host, "structured", json!({})).await.content,
        "{\"answer\":42}"
    );
    assert_eq!(
        invoke(&host, "image", json!({})).await.content,
        "[MCP image: 4 bytes omitted]"
    );
    let binary = invoke(&host, "binary", json!({})).await.content;
    assert!(binary.contains("audio: 1 bytes omitted"));
    assert!(binary.contains("resource:"));
    assert!(!binary.contains("YWJjZA"));
    let log = requests(&log);
    assert_eq!(log[0]["request"]["method"], "initialize");
    assert_eq!(log[0]["request"]["params"]["protocolVersion"], "2025-06-18");
    assert_eq!(log[1]["request"]["method"], "notifications/initialized");
    assert_eq!(
        log.iter()
            .filter(|line| line["request"]["method"] == "tools/list")
            .count(),
        2
    );
    host.shutdown(Duration::from_secs(2)).await;
}
