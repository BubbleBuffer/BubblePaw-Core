//! A summary fork must keep the main request's wire prefix under the default
//! tool-loop reasoning replay, when the capture ends inside an open tool loop.
use anyhow::Result;
use axum::{Json, Router, extract::State, routing::post};
use bubblepaw_core::{
    CancellationToken, Message, ModelRequest, Provider, Role, ToolCall, ToolDefinition,
};
use bubblepaw_runner::{
    episode_models::{
        EpisodeFork, EpisodeModelConfig, EpisodeModels, HttpEpisodeModels, SummaryInput,
    },
    model_roles::ModelRoles,
    provider::{ChatCompletionsProvider, ProviderRegistry},
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

type Wire = Arc<Mutex<(Vec<Value>, VecDeque<Value>)>>;

async fn receive(State(wire): State<Wire>, Json(body): Json<Value>) -> Json<Value> {
    let mut wire = wire.lock().unwrap();
    wire.0.push(body);
    Json(wire.1.pop_front().expect("unexpected network call"))
}

fn reply(text: &str) -> Value {
    json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":text}}],"usage":{"prompt_tokens":501,"completion_tokens":23}})
}

fn message(id: &str, role: Role, content: &str) -> Message {
    Message {
        id: id.into(),
        session_id: "source-session".into(),
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

#[tokio::test]
async fn fork_keeps_reasoning_of_an_open_tool_loop_in_the_shared_prefix() -> Result<()> {
    let wire: Wire = Arc::new(Mutex::new((
        vec![],
        VecDeque::from([
            reply("main warm"),
            reply("# Recorded fix\nFailure remains [source-t] [call-exact].\nOpen: repair"),
        ]),
    )));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let app = Router::new()
        .route("/chat/completions", post(receive))
        .with_state(wire.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // Default reasoning replay (tool loop): no explicit policy for the model.
    let provider = ChatCompletionsProvider::without_key(base)?.with_opencode_route();
    let registry = Arc::new(ProviderRegistry::new("opencode", Arc::new(provider)));

    let mut assistant = message("source-a", Role::Assistant, "Checking the result.");
    assistant.tool_calls.push(ToolCall {
        id: "call-exact".into(),
        name: "js".into(),
        arguments: json!({"code":"return await tools.check()"}),
    });
    assistant.provider_state = Some(bubblepaw_core::with_provider_state_origin(
        json!({"adapter":"chat-completions","rounds":[
            {"text_range":[0,20],"message_fields":{"reasoning_content":"Loop thought"}},
        ]}),
        "chat-completions",
        Some("flash"),
        "native",
    )?);
    let mut tool = message("source-t", Role::Tool, "MIDDLE_EXACT_FAILURE exit=7");
    tool.tool_call_id = Some("call-exact".into());
    let request = ModelRequest {
        model: "opencode/flash".into(),
        system_prompt: "Frozen original opening instructions".into(),
        messages: vec![
            message("source-u", Role::User, "Fix the recorded failure"),
            assistant,
            tool,
        ],
        tools: vec![ToolDefinition {
            name: "js".into(),
            description: "Frozen js declaration".into(),
            input_schema: json!({"type":"object"}),
            read_only: false,
            approval: Default::default(),
        }],
        session_id: "source-session".into(),
        ..Default::default()
    };
    registry
        .generate(request.clone(), Arc::new(|_| {}), CancellationToken::new())
        .await?;
    let roles = ModelRoles::for_models("opencode/flash".into(), "opencode/flash".into());
    let models = HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles,
            retry_base_delay: Duration::ZERO,
        },
        registry.clone(),
        None,
    )?;
    models
        .summarize_fork(
            SummaryInput {
                focus: None,
                session_id: "source-session".into(),
                previous_summary: None,
                turn_request: Some("Fix the recorded failure".into()),
                episode_text: "unused".into(),
                cap: 2000,
            },
            EpisodeFork {
                request,
                first_message_id: "source-u".into(),
                last_message_id: "source-t".into(),
                provider: registry,
                max_request_bytes: 2 * 1024 * 1024,
                context_window: Some(200_000),
                output_reserve_tokens: 4000,
                dispatched: true,
            },
        )
        .await?;
    server.abort();
    let rows = wire.lock().unwrap().0.clone();
    let main = rows[0]["messages"].as_array().unwrap();
    let branch = rows[1]["messages"].as_array().unwrap();
    assert!(
        main.iter()
            .any(|m| m["reasoning_content"] == "Loop thought"),
        "the main request replays the open loop's reasoning"
    );
    assert_eq!(&branch[..main.len()], main.as_slice());
    assert_eq!(branch.len(), main.len() + 2);
    Ok(())
}
