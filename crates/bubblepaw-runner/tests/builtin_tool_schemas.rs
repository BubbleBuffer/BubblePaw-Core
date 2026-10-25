//! Every built-in tool advertises an object input schema. Native function tools
//! and MCP (the claude-code extension's `assertObjectSchema`) reject anything
//! else, so one bare-array schema fails every run that registers that tool.
use bubblepaw_core::{ExecutionMode, Harness, HarnessConfig, Store, ToolRegistry};
use bubblepaw_runner::{
    agent_handles::register_agent_handles,
    agents::AgentTools,
    artifacts::ArtifactStore,
    codemode::CodeMode,
    net::{WebConfig, register_fetch_search_tools, register_research_sources},
    provider::ScriptedProvider,
    question::QuestionTools,
    todo, tools,
    wake::WakeCoordinator,
};
use std::{collections::BTreeSet, sync::Arc, time::Duration};

/// Checks the native view (host tools promoted, discovery tools added), the
/// JavaScript view (`js`, which the native view drops) and every host
/// capability; returns the distinct names checked.
fn assert_object_schemas(label: &str, registry: &ToolRegistry) -> BTreeSet<String> {
    let native = registry.with_execution_mode(ExecutionMode::Native);
    let js = registry.with_execution_mode(ExecutionMode::Js);
    let mut names = BTreeSet::new();
    for definition in native
        .definitions()
        .into_iter()
        .chain(js.definitions())
        .chain(registry.capabilities())
    {
        assert_eq!(
            definition.input_schema["type"], "object",
            "{label}: tool {} advertises a non-object input schema {}",
            definition.name, definition.input_schema
        );
        names.insert(definition.name);
    }
    names
}

#[tokio::test]
async fn every_builtin_tool_input_schema_is_an_object() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::in_memory().unwrap();
    let mode = Arc::new(
        CodeMode::new(
            root.path().to_owned(),
            Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap()),
        )
        .unwrap(),
    );

    let mut host = ToolRegistry::new();
    let processes = tools::register_native_tools(&mut host, root.path(), false).unwrap();
    tools::register_history_tools(&mut host, store.clone()).unwrap();
    todo::register(&mut host, store.clone()).unwrap();
    let wake = Arc::new(WakeCoordinator::new(false, Duration::ZERO));
    Arc::new(QuestionTools::new(store.clone(), wake, true))
        .register(&mut host)
        .unwrap();
    // Registers the environment tools and `js`.
    mode.register(&mut host).unwrap();
    let child = Harness::new(
        store.clone(),
        Arc::new(ScriptedProvider::new("done")),
        host.read_only(),
        HarnessConfig::default(),
    );
    let agents = Arc::new(
        AgentTools::new(store.clone(), child, None)
            .with_repl(mode.clone())
            .with_processes(processes),
    );
    agents.register(&mut host).unwrap();
    register_agent_handles(&mut host, agents).unwrap();
    let names = assert_object_schemas("host", &host);
    for expected in ["todo", "read", "exec", "question", "history_search", "js"] {
        assert!(
            names.contains(expected),
            "{expected} missing from {names:?}"
        );
    }

    let mut fetch = ToolRegistry::new();
    register_fetch_search_tools(&mut fetch, &WebConfig::default()).unwrap();
    assert!(!assert_object_schemas("fetch/search", &fetch).is_empty());
    let mut research = ToolRegistry::new();
    register_research_sources(&mut research, &WebConfig::default()).unwrap();
    assert!(!assert_object_schemas("research sources", &research).is_empty());
}
