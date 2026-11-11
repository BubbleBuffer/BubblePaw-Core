//! Required results through the real native/QuickJS dispatch paths and delegate consumer.
use bubblepaw_core::{
    ExecutionMode, Harness, HarnessConfig, JsonSchema, Role, RunOptions, Store, ToolCall,
    ToolRegistry,
};
use bubblepaw_runner::{
    artifacts::ArtifactStore,
    codemode::CodeMode,
    delegate::{self, DelegateRun, assignment::Assignment, ledger::Ledger, routing::RouteChoice},
    provider::{ScriptedProvider, ScriptedStep},
    tools::register_native_tools,
};
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn required_result_uses_real_js_wrapper_and_schema_errors_are_retryable() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::in_memory().unwrap();
    let session = store.create_session("structured JS").unwrap().id;
    let mut tools = ToolRegistry::new();
    let _processes = register_native_tools(&mut tools, root.path(), false).unwrap();
    let repl = Arc::new(
        CodeMode::new(
            root.path().to_owned(),
            Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap()),
        )
        .unwrap(),
    );
    repl.register(&mut tools).unwrap();
    let steps = [
        "await tools.submit_result({answer:3});",
        "await tools.submit_result({answer:'ready'}); await tools.write({path:'after.txt',content:'cell finished'}); try { await tools.submit_result({answer:'duplicate'}); } catch (_) {}",
    ]
    .into_iter()
    .enumerate()
    .map(|(index, code)| ScriptedStep::Calls {
        calls: vec![ToolCall {
            id: format!("js-{index}"),
            name: "js".into(),
            arguments: json!({"code":code}),
        }],
    })
    .collect();
    let harness = Harness::new(
        store.clone(),
        Arc::new(ScriptedProvider::with_steps(steps, "unexpected")),
        tools,
        HarnessConfig {
            execution_mode: ExecutionMode::Js,
            ..Default::default()
        },
    );
    let options = RunOptions { output_schema: Some(JsonSchema::new("answer", json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false})).unwrap()), ..Default::default() };
    let definitions = harness
        .effective_tool_definitions(&session, &options)
        .unwrap();
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0].name, "js");
    assert!(definitions[0].description.contains("submit_result"));
    let run = harness
        .prompt(&session, "Return the result", options)
        .await
        .unwrap();
    assert_eq!(run.status, "completed", "{run:?}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("after.txt")).unwrap(),
        "cell finished"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(run.text.as_deref().unwrap()).unwrap(),
        json!({"answer":"ready"})
    );
    let messages = store.messages(&session).unwrap();
    assert!(
        messages.iter().any(|m| m.role == Role::Tool && m.is_error),
        "invalid submission must be a retryable tool error"
    );
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.tool_calls.iter().any(|c| c.name == "js"))
            .count(),
        2
    );
}

#[tokio::test]
async fn native_delegate_consumes_submitted_report_and_runs_acceptance_check() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::in_memory().unwrap();
    let report = json!({"status":"done","summary":"reported through a tool","checks_claimed":[],"open_questions":[]});
    let provider = ScriptedProvider::with_steps(
        vec![ScriptedStep::Calls {
            calls: vec![ToolCall {
                id: "final-report".into(),
                name: "submit_result".into(),
                arguments: report.clone(),
            }],
        }],
        "unexpected",
    );
    let harness = Arc::new(Harness::new(
        store.clone(),
        Arc::new(provider),
        ToolRegistry::new(),
        HarnessConfig::default(),
    ));
    let mut ledger = Ledger::open(&root.path().join("ledger.db")).unwrap();
    let result = delegate::run(harness, &store, &mut ledger, DelegateRun {
        assignment: Assignment::parse("+++\ncategory = \"research\"\nchecks = [\"exit 0\"]\nroute = { class = \"small\" }\n+++\nReport completion.\n").unwrap(),
        route: RouteChoice { model: "scripted".into(), class: Some("small".into()), reason: "test".into() },
        workspace: root.path().to_owned(), title: "native delegate".into(), strategy: Default::default(), events: None, collector: None,
    }).await.unwrap();
    assert_eq!(result.status, "accepted", "{result:?}");
    assert_eq!(result.summary, report["summary"]);
    assert_eq!(result.checks.len(), 1);
    let messages = store.messages(&result.session_id).unwrap();
    assert!(messages.iter().any(|m| {
        m.tool_calls
            .iter()
            .any(|c| c.name == "submit_result" && c.arguments == report)
    }));
    assert!(
        messages
            .iter()
            .any(|m| m.role == Role::User && m.content.contains("call `submit_result`"))
    );
}
