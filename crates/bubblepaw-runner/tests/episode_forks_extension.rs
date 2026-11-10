//! Summary forks on a provider extension: only an extension that declares
//! `summary_fork` is forked, as a `summary_fork` request holding the main
//! conversation up to the cut plus one user message carrying both controls.
use anyhow::Result;
use bubblepaw_core::{
    CancellationToken, Message, ModelRequest, Provider, Role, Store, ToolCall, ToolDefinition,
};
use bubblepaw_runner::{
    episode_models::{
        EpisodeFork, EpisodeModelConfig, EpisodeModels, HttpEpisodeModels, SummaryInput,
    },
    model_roles::ModelRoles,
    provider::{ExtensionConfig, ExtensionProvider, ProviderRegistry},
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

const SUMMARY: &str = "# Recorded fix\nFailure remains [source-t2] [call-2].\nOpen: repair";

fn message(session: &str, id: &str, role: Role, content: &str) -> Message {
    Message {
        id: id.into(),
        session_id: session.into(),
        role,
        origin: None,
        content: content.into(),
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: String::new(),
    }
}

/// A main request captured mid tool loop, with one more message after the cut.
fn request(session: &str) -> ModelRequest {
    let mut messages = vec![message(session, "source-u", Role::User, "Fix the failure")];
    for index in 1..=2 {
        let mut assistant = message(session, &format!("source-a{index}"), Role::Assistant, "");
        assistant.tool_calls.push(ToolCall {
            id: format!("call-{index}"),
            name: "js".into(),
            arguments: json!({}),
        });
        let mut tool = message(session, &format!("source-t{index}"), Role::Tool, "result");
        tool.tool_call_id = Some(format!("call-{index}"));
        messages.extend([assistant, tool]);
    }
    messages.push(message(session, "source-later", Role::User, "FUTURE"));
    ModelRequest {
        model: "p/code-model".into(),
        system_prompt: "Frozen original opening instructions".into(),
        messages,
        tools: vec![ToolDefinition {
            name: "js".into(),
            description: "Frozen js declaration".into(),
            input_schema: json!({"type":"object"}),
            read_only: false,
            approval: Default::default(),
        }],
        max_output_tokens: Some(8000),
        session_id: session.into(),
        ..Default::default()
    }
}

fn extension(capabilities: Option<Value>, log: &std::path::Path) -> ExtensionProvider {
    ExtensionProvider::new(ExtensionConfig {
        id: "fake".into(),
        command: env!("CARGO_BIN_EXE_bubblepaw-fake-extension").into(),
        args: capabilities
            .map(|value| vec![format!("--capabilities={value}")])
            .unwrap_or_default(),
        env: [
            ("BP_FAKE_REPLY".to_owned(), SUMMARY.to_owned()),
            ("BP_FAKE_STEP_LOG".to_owned(), log.display().to_string()),
        ]
        .into(),
        ..Default::default()
    })
}

/// Sends the request as the main call, then summarizes it up to `source-t2` as
/// a fork. Returns the fork's outcome and the requests the extension logged.
async fn fork(capabilities: Option<Value>) -> Result<(Result<()>, Vec<Value>)> {
    let directory = tempfile::tempdir()?;
    let log = directory.path().join("steps.jsonl");
    let store = Store::in_memory()?;
    let session = store.create_session("Fork extension")?.id;
    let request = request(&session);
    let registry = Arc::new(ProviderRegistry::new(
        "p",
        Arc::new(extension(capabilities, &log)),
    ));
    registry.set_usage_store(store.clone());
    store
        .cache_accounting()
        .context
        .build(request.clone(), &session, "root", &[]);
    registry
        .generate(request.clone(), Arc::new(|_| {}), CancellationToken::new())
        .await?;
    let models = HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles: ModelRoles::for_models(request.model.clone(), request.model.clone()),
            retry_base_delay: Duration::ZERO,
        },
        registry.clone(),
        None,
    )?;
    models.record_fork_context(&session, &request, &[]);
    let outcome = models
        .summarize_fork(
            SummaryInput {
                focus: None,
                session_id: session,
                previous_summary: None,
                turn_request: None,
                episode_text: "unused".into(),
                cap: 2000,
            },
            EpisodeFork {
                request,
                first_message_id: "source-u".into(),
                last_message_id: "source-t2".into(),
                provider: registry,
                max_request_bytes: 2 * 1024 * 1024,
                context_window: Some(200_000),
                output_reserve_tokens: 4000,
                dispatched: true,
            },
        )
        .await
        .map(|_| ());
    let logged = std::fs::read_to_string(log)?
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    Ok((outcome, logged))
}

#[tokio::test]
async fn an_extension_declaring_summary_fork_is_forked_with_one_control_message() -> Result<()> {
    let (outcome, logged) = fork(Some(
        json!({"summary_fork":true,"input_overhead_tokens":4000}),
    ))
    .await?;
    outcome?;
    assert_eq!(logged[0]["method"], "step");
    assert_eq!(logged[1]["method"], "summary_fork");
    let (main, branch) = (&logged[0]["params"], &logged[1]["params"]);
    let ids = |params: &Value| -> Vec<String> {
        params["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| message["id"].as_str().unwrap_or("").to_owned())
            .collect()
    };
    // The prefix up to the cut, nothing after it, then one message without an id.
    let mut expected = ids(main);
    expected.truncate(5);
    expected.push(String::new());
    assert_eq!(ids(branch), expected);
    assert_eq!(branch["session_id"], main["session_id"]);
    let last = branch["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(last["role"], "user");
    let control = last["content"].as_str().unwrap();
    let first = control.find("EPISODE MEMORY BRANCH").unwrap();
    let second = control
        .find("HOST-GENERATED SUMMARY RETURN CONTROL")
        .unwrap();
    assert!(first < second);
    assert!(control.contains(r#""protocol":"claude-code""#));
    assert!(control.contains(r#""wire_positions":[3]"#));
    assert!(!control.contains("FUTURE"));
    Ok(())
}

#[tokio::test]
async fn an_extension_without_summary_fork_is_never_forked() -> Result<()> {
    let (outcome, logged) = fork(None).await?;
    // Whatever the host does instead, it never sends a summary_fork request.
    let _ = outcome;
    assert!(logged.iter().all(|line| line["method"] == "step"));
    Ok(())
}
