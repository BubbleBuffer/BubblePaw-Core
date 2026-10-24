//! A native delegated-style result-tool run remains usable after real context
//! pressure gathers its first completed turn and applies the resulting memory.
use super::*;
use bubblepaw_core::{
    ExecutionMode, Harness, HarnessConfig, JsonSchema, RunOptions, Store, ToolRegistry,
};
use bubblepaw_runner::{
    episode_worker::EpisodeWorker,
    summarization::{SummarizingContext, SummaryConfig},
};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct PressureWire {
    requests: Mutex<Vec<Value>>,
    main_calls: AtomicUsize,
}

async fn pressure_reply(
    State(wire): State<Arc<PressureWire>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let messages = body["messages"].as_array().unwrap();
    let summary = messages.iter().any(|message| {
        message["content"]
            .as_str()
            .is_some_and(|text| text.starts_with("EPISODE MEMORY BRANCH"))
    });
    let chapter = messages.iter().any(|message| {
        message["content"]
            .as_str()
            .is_some_and(|text| text.starts_with("<harness kind=\"chapter_task\">"))
    });
    let decision = messages.first().is_some_and(|message| {
        message["content"]
            .as_str()
            .is_some_and(|text| text.contains("Allowed options:"))
    });
    wire.requests.lock().unwrap().push(body);
    Json(if summary {
        native::submit(
            "submit_summary",
            "# Delegated evidence\nThe delegated task produced exact evidence; implementation remains pending.\nOpen: complete the delegated task",
        )
    } else if chapter {
        native::submit(
            "submit_chapter",
            "# Delegated work\nThe delegated task produced evidence.\nRelated: none",
        )
    } else if decision {
        reply("{\"choice\":\"none\"}")
    } else {
        let index = wire.main_calls.fetch_add(1, Ordering::SeqCst);
        let first = index == 0;
        let mut response = native::delivery(
            "submit_result",
            json!({
                "done": !first,
                "evidence": if first { " datum".repeat(20_000) } else { "Completed after memory apply".into() },
            }),
        );
        response["choices"][0]["message"]["tool_calls"][0]["id"] =
            format!("main-result-{index}").into();
        response
    })
}

#[tokio::test]
async fn delegated_native_result_tool_run_crosses_pressure_and_completes_after_apply() -> Result<()>
{
    let wire = Arc::new(PressureWire::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let app = Router::new()
        .route("/chat/completions", post(pressure_reply))
        .with_state(wire.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // Reuse Fixture's abort-on-drop behavior without exposing another live task.
    let transport = Arc::new(ProviderRegistry::new(
        "opencode",
        Arc::new(ChatCompletionsProvider::without_key(base)?),
    ));
    let _server_guard = Fixture {
        wire: Arc::new(Wire::default()),
        registry: transport.clone(),
        server,
    };
    let store = Store::in_memory()?;
    let root = store.create_session("delegation parent")?;
    let session = store.create_session_with_visibility(
        "native schema child",
        Default::default(),
        Some(&root.id),
        true,
    )?;
    let mut config = SummaryConfig {
        threshold_cap: 16_000,
        ..Default::default()
    };
    config.lcm.child_compact_threshold_tokens = Some(16_000);
    config.lcm.output_reserve_tokens = 0;
    config.lcm.raw_tail_tokens = 64;
    config.lcm.task_anchor_tokens = 64;
    config.lcm.episodes.source_target_tokens = 64;
    config.lcm.episodes.source_max_tokens = 32_000;
    let context = Arc::new(SummarizingContext::new(
        store.clone(),
        transport.clone(),
        config.clone(),
        None,
    )?);
    let worker = EpisodeWorker::new_with_config(
        store.clone(),
        Arc::new(models(transport.clone(), None)?),
        context.clone(),
        config.lcm.episodes,
    )?;
    context.set_episode_worker(&worker);
    let harness = Harness::new(
        store.clone(),
        transport,
        ToolRegistry::new(),
        HarnessConfig {
            model: "opencode/flash".into(),
            execution_mode: ExecutionMode::Native,
            context: context.clone(),
            ..Default::default()
        },
    );
    let schema = JsonSchema::new(
        "delegated_result",
        json!({
            "type":"object", "properties":{"done":{"type":"boolean"},"evidence":{"type":"string"}},
            "required":["done","evidence"], "additionalProperties":false
        }),
    )?;
    let options = RunOptions {
        origin: MessageOrigin::ChildTask,
        output_schema: Some(schema),
        ..Default::default()
    };
    let first_run = harness
        .prompt(
            &session.id,
            "Collect evidence for the delegated task",
            options.clone(),
        )
        .await?;
    let before = store.messages(&session.id)?;
    assert_eq!(
        serde_json::from_str::<Value>(first_run.text.as_deref().expect("completed result"))?["done"],
        false
    );
    let evidence = before
        .iter()
        .find(|message| {
            message.tool_calls.iter().any(|call| {
                call.name == "submit_result"
                    && call.arguments["evidence"]
                        .as_str()
                        .is_some_and(|text| text.len() > 100_000)
            })
        })
        .expect("the large submitted evidence must remain in authoritative history");
    assert!(store.context_episodes(&session.id)?.is_empty());
    let final_run = harness
        .prompt(
            &session.id,
            "Complete the delegated task using its retained evidence",
            options,
        )
        .await?;
    let messages = store.messages(&session.id)?;
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.origin == Some(MessageOrigin::ChildTask))
            .count(),
        2
    );
    assert_eq!(
        serde_json::from_str::<Value>(final_run.text.as_deref().expect("completed result"))?["done"],
        true
    );
    let episodes = store.context_episodes(&session.id)?;
    assert!(!episodes.is_empty(), "pressure must gather valid memory");
    assert!(
        episodes
            .iter()
            .all(|episode| episode.summary.contains("Delegated evidence"))
    );
    assert!(
        store.context_view(&session.id)?.unwrap().frontier.is_some(),
        "memory must be applied before main continuation"
    );
    // Branch memory and controls are transient. The original large reply and
    // delegated task instructions remain authoritative and exactly retrievable.
    assert_eq!(
        messages.iter().find(|message| message.id == evidence.id),
        Some(evidence)
    );
    assert!(
        !messages
            .iter()
            .any(|message| message.content.contains("EPISODE MEMORY BRANCH")
                || message.content.contains("submit_summary"))
    );
    let requests = wire.requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .all(|body| body.get("response_format").is_none())
    );
    let mains: Vec<_> = requests
        .iter()
        .filter(|body| {
            body["tools"].as_array().is_some_and(|tools| {
                tools
                    .iter()
                    .any(|tool| tool["function"]["name"] == "submit_result")
            }) && !body["messages"].as_array().unwrap().iter().any(|message| {
                message["content"].as_str().is_some_and(|text| {
                    text.starts_with("EPISODE MEMORY BRANCH")
                        || text.starts_with("<harness kind=\"chapter_task\">")
                })
            })
        })
        .collect();
    assert_eq!(mains.len(), 2);
    let summaries: Vec<_> = requests
        .iter()
        .filter(|body| {
            body["messages"].as_array().unwrap().iter().any(|message| {
                message["content"]
                    .as_str()
                    .is_some_and(|text| text.starts_with("EPISODE MEMORY BRANCH"))
            })
        })
        .collect();
    assert!(!summaries.is_empty());
    for branch in summaries {
        assert_eq!(branch["tools"], mains[0]["tools"]);
        let declarations = branch["tools"].to_string();
        assert!(declarations.contains("call_tool"));
        assert!(declarations.contains("submit_result"));
        assert!(
            !declarations.contains("submit_summary") && !declarations.contains("submit_chapter")
        );
    }
    assert!(
        mains[1]["messages"].to_string().len() < mains[0]["messages"].to_string().len() + 10_000
    );
    assert!(
        mains[1]["messages"]
            .to_string()
            .contains("Delegated evidence")
    );
    Ok(())
}
