use anyhow::Result;
use async_trait::async_trait;
use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
use bubblepaw_core::{
    CancellationToken, Message, MessageOrigin, ModelRequest, ModelResponse, Provider,
    ProviderError, ProviderErrorKind, ReasoningEffort, ReasoningReplay, Role, TextCallback,
    ToolCall, ToolDefinition,
};
use bubblepaw_runner::{
    episode_models::{
        EpisodeFork, EpisodeModelConfig, EpisodeModels, HttpEpisodeModels, SummaryInput,
    },
    model_roles::ModelRoles,
    provider::{ChatCompletionsProvider, ProviderRegistry, decision_error_usage},
    trace::{JsonlTrace, TraceConfig},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

#[path = "episode_forks_http/fixture.rs"]
mod fixture;
use fixture::*;

#[path = "episode_forks_http/native.rs"]
mod native;
#[path = "episode_forks_http/pressure.rs"]
mod pressure;

#[tokio::test]
async fn serialized_fork_preserves_native_prefix_tools_route_and_main_continuation() -> Result<()> {
    let f = fixture(vec![
        reply("main warm"),
        reply("# Recorded fix\nFailure remains [source-t] [call-exact].\nOpen: repair"),
        reply("main continues"),
    ])
    .await?;
    let original = request();
    let saved = serde_json::to_value(&original)?;
    f.registry
        .generate(original.clone(), Arc::new(|_| {}), CancellationToken::new())
        .await?;
    let summary = models(f.registry.clone(), None)?
        .summarize_fork(input(), fork(f.registry.clone(), original.clone()))
        .await?;
    assert_eq!(summary.title, "Recorded fix");
    assert_eq!(summary.usage.cache_read_tokens, 400);
    assert_eq!(serde_json::to_value(&original)?, saved);
    f.registry
        .generate(original.clone(), Arc::new(|_| {}), CancellationToken::new())
        .await?;
    let rows = f.wire.requests.lock().unwrap();
    let (main_headers, main) = &rows[0];
    let (branch_headers, branch) = &rows[1];
    assert_eq!(branch["model"], main["model"]);
    assert_eq!(branch["tools"], main["tools"]);
    assert_eq!(branch.get("tool_choice"), main.get("tool_choice"));
    assert_eq!(branch["reasoning_effort"], main["reasoning_effort"]);
    assert_eq!(branch["reasoning_effort"], "high");
    assert_eq!(branch["max_tokens"], main["max_tokens"]);
    assert_eq!(branch["max_tokens"], 8000);
    assert_eq!(
        main_headers.get("x-opencode-session"),
        branch_headers.get("x-opencode-session")
    );
    assert_eq!(branch_headers["x-opencode-session"], "source-session");
    let prefix = main["messages"].as_array().unwrap();
    let messages = branch["messages"].as_array().unwrap();
    assert_eq!(&messages[..prefix.len()], prefix);
    assert_eq!(messages.len(), prefix.len() + 2);
    assert_eq!(messages[prefix.len()]["role"], "system");
    let control = messages[prefix.len()]["content"].as_str().unwrap();
    assert!(control.contains("source-u through source-t"));
    assert!(control.contains(r#""source_id":"source-a""#));
    assert!(control.contains(r#""wire_positions":[2,3]"#));
    assert!(control.contains("call-exact"));
    assert!(control.contains("Only the selected mapped source rows supply provenance"));
    assert!(control.contains("NOT source events or human requests"));
    assert!(control.contains("Never describe this scaffolding as a human redirect"));
    assert!(control.contains("Write the changes within the selected range"));
    assert!(control.contains("briefly labeled prior context"));
    assert!(
        control
            .contains("Wire or prepared positions quoted inside previous summaries are historical")
    );
    assert!(control.contains("never store wire or prepared position numbers"));
    assert!(control.contains("Do not relabel an assistant acknowledgment as a typed request"));
    assert!(control.contains("END is a snapshot boundary, not an interruption"));
    assert!(control.contains("not yet observed by END"));
    assert!(control.contains(
        "do not infer that summarization interrupted, cancelled or redirected the main task"
    ));
    let return_control = messages.last().unwrap()["content"].as_str().unwrap();
    assert!(return_control.starts_with("HOST-GENERATED SUMMARY RETURN CONTROL"));
    assert!(return_control.contains("not a source event or human request"));
    assert!(messages.iter().any(|message| {
        message["content"]
            .as_str()
            .is_some_and(|text| text.contains("MIDDLE_EXACT_FAILURE exit=7"))
    }));
    assert!(!branch.to_string().contains("BOUNDED_INPUT_SENTINEL"));
    assert_eq!(
        rows[2].1, *main,
        "branch controls never mutate the main continuation"
    );
    Ok(())
}

#[tokio::test]
async fn fork_output_allowance_preserves_main_default_unless_summary_role_overrides() -> Result<()>
{
    for (main_limit, role_limit, expected) in [
        (None, None, None::<u32>),
        (Some(8000), None, Some(8000)),
        (None, Some(6000), Some(6000)),
        (Some(8000), Some(6000), Some(6000)),
    ] {
        let f = fixture(vec![reply(
            "# Source facts\nOnly selected work.\nOpen: none",
        )])
        .await?;
        let mut original = request();
        original.max_output_tokens = main_limit;
        let mut roles = ModelRoles::for_models("opencode/flash".into(), "opencode/flash".into());
        roles.summary.max_output_tokens = role_limit;
        let models = HttpEpisodeModels::from_registry(
            EpisodeModelConfig {
                roles,
                retry_base_delay: Duration::ZERO,
            },
            f.registry.clone(),
            None,
        )?;
        let mut summary_input = input();
        summary_input.cap = 400;
        summary_input.previous_summary = Some("PREVIOUS_SUMMARY_CONTEXT_ONLY".into());
        let captured = fork(f.registry.clone(), original);
        models.summarize_fork(summary_input, captured).await?;
        let rows = f.wire.requests.lock().unwrap();
        assert_eq!(rows.len(), 1);
        let body = &rows[0].1;
        assert_eq!(
            body.get("max_tokens").and_then(Value::as_u64),
            expected.map(u64::from)
        );
        let messages = body["messages"].as_array().unwrap();
        let control = messages[messages.len() - 2]["content"].as_str().unwrap();
        assert!(control.contains("at most 400 tokens"));
        assert!(control.contains(
            "previous summary, final summary-return request and any continuation controls"
        ));
        assert!(control.contains(
            "PREVIOUS EPISODE (read-only context; not selected source events or a new request)"
        ));
        assert!(control.contains("PREVIOUS_SUMMARY_CONTEXT_ONLY"));
        assert!(
            messages.last().unwrap()["content"]
                .as_str()
                .unwrap()
                .contains("Only selected mapped source rows supply event and request provenance")
        );
    }
    Ok(())
}

#[tokio::test]
async fn configured_output_reservation_bounds_fork_when_no_request_limit_exists() -> Result<()> {
    let f = fixture(vec![]).await?;
    let mut original = request();
    original.max_output_tokens = None;
    let mut captured = fork(f.registry.clone(), original);
    captured.output_reserve_tokens = 200_000;
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("trace.jsonl");
    let trace = JsonlTrace::open(TraceConfig {
        path: path.clone(),
        ..Default::default()
    })?;
    let mut summary_input = input();
    summary_input.cap = 400;
    models(f.registry.clone(), Some(trace.clone()))?
        .summarize_fork(summary_input, captured)
        .await
        .unwrap_err();
    trace.flush(Duration::from_secs(2))?;
    let events = std::fs::read_to_string(path)?;
    assert!(events.contains("episode_fork_unavailable"));
    assert!(events.contains("context_window_limit"));
    let rows = f.wire.requests.lock().unwrap();
    assert!(
        rows.is_empty(),
        "capacity failure must not substitute episode input"
    );
    Ok(())
}

#[tokio::test]
async fn fork_mapping_preserves_typed_generated_and_child_task_provenance() -> Result<()> {
    let f = fixture(vec![reply("# Source provenance\nRecorded.\nOpen: none")]).await?;
    let mut original = request();
    original.messages[0].origin = Some(MessageOrigin::Typed);
    let mut generated = message(
        "source-generated",
        Role::User,
        "[host notice] canonical inventory unchanged",
    );
    generated.origin = Some(MessageOrigin::Generated);
    let mut child = message(
        "source-child-task",
        Role::User,
        "Continue the assigned delegated work",
    );
    child.origin = Some(MessageOrigin::ChildTask);
    original.messages.insert(1, generated);
    original.messages.insert(2, child);
    models(f.registry.clone(), None)?
        .summarize_fork(input(), fork(f.registry.clone(), original))
        .await?;
    let rows = f.wire.requests.lock().unwrap();
    let messages = rows[0].1["messages"].as_array().unwrap();
    let control = messages[messages.len() - 2]["content"].as_str().unwrap();
    let mapping_json = control
        .split_once(
            "Exact selected source-message/tool-call mapping to rendered input positions:\n",
        )
        .unwrap()
        .1
        .split_once("\n\n")
        .unwrap()
        .0;
    let mapping: Value = serde_json::from_str(mapping_json)?;
    let sources = mapping["sources"].as_array().unwrap();
    for (id, origin, typed) in [
        ("source-u", "typed", true),
        ("source-generated", "generated", false),
        ("source-child-task", "child_task", false),
    ] {
        let source = sources
            .iter()
            .find(|source| source["source_id"] == id)
            .unwrap();
        assert_eq!(source["origin"], origin);
        assert_eq!(source["is_typed_user"], typed);
    }
    assert!(
        control.contains("Delegated tasks and generated host controls are never human requests")
    );
    assert_eq!(
        messages[2]["content"],
        "[host notice] canonical inventory unchanged"
    );
    assert_eq!(
        messages[3]["content"],
        "Continue the assigned delegated work"
    );
    assert!(
        messages[..messages.len() - 2].iter().all(
            |message| message.get("origin").is_none() && message.get("is_typed_user").is_none()
        )
    );
    Ok(())
}

#[tokio::test]
async fn a_second_returned_tool_call_fails_after_one_refusal_and_both_are_billed() -> Result<()> {
    let returned = json!({"choices":[{"finish_reason":"tool_calls","message":{"content":null,"tool_calls":[{"id":"new-call","type":"function","function":{"name":"js","arguments":"{\"code\":\"throw 1\"}"}}]}}],"usage":{"prompt_tokens":111,"completion_tokens":17,"prompt_tokens_details":{"cached_tokens":100,"cache_creation_tokens":7},"completion_tokens_details":{"reasoning_tokens":12}}});
    let f = fixture(vec![returned.clone(), returned]).await?;
    let error = models(f.registry.clone(), None)?
        .summarize_fork(input(), fork(f.registry.clone(), request()))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("tool_call_returned"));
    let usage = decision_error_usage(&error).unwrap();
    assert_eq!(usage.input_tokens, 222);
    assert_eq!(usage.output_tokens, 34);
    let provider_error = ProviderError::of(&error).unwrap();
    assert_eq!(provider_error.kind, ProviderErrorKind::Fatal);
    assert!(!provider_error.published);
    let full_usage = provider_error.usage.as_ref().unwrap();
    assert_eq!(full_usage.cache_read_tokens, 200);
    assert_eq!(full_usage.cache_creation_tokens, 14);
    assert_eq!(full_usage.reasoning_tokens, Some(24));
    assert_eq!(
        f.wire.requests.lock().unwrap().len(),
        2,
        "one refusal retry, and no tool effect occurs"
    );
    Ok(())
}

#[tokio::test]
async fn unknown_window_still_forks_and_future_source_is_excluded() -> Result<()> {
    let f = fixture(vec![reply("# Range\nRecorded.\nOpen: none")]).await?;
    let mut source = request();
    source.messages.insert(
        0,
        message("source-old", Role::User, "OLDER_CONTEXT_SENTINEL"),
    );
    source
        .messages
        .push(message("future", Role::User, "FUTURE_EVENT_MUST_NOT_LEAK"));
    let mut captured = fork(f.registry.clone(), source);
    captured.context_window = None;
    models(f.registry.clone(), None)?
        .summarize_fork(input(), captured)
        .await?;
    let rows = f.wire.requests.lock().unwrap();
    assert!(!rows[0].1.to_string().contains("FUTURE_EVENT_MUST_NOT_LEAK"));
    assert!(rows[0].1.to_string().contains("OLDER_CONTEXT_SENTINEL"));
    let messages = rows[0].1["messages"].as_array().unwrap();
    let selector = messages[messages.len() - 2]["content"].as_str().unwrap();
    assert!(!selector.contains("source-old"));
    assert!(selector.contains(r#""source_id":"source-u""#));
    assert_eq!(
        rows[0].1["messages"][0]["content"],
        "Frozen original opening instructions"
    );
    Ok(())
}

#[tokio::test]
async fn actual_window_or_byte_limit_has_categorical_failure_without_episode_substitution()
-> Result<()> {
    for (window, physical_window, bytes, reason) in [
        (Some(10), None, 2 * 1024 * 1024, "context_window_limit"),
        (
            Some(200_000),
            Some(10),
            2 * 1024 * 1024,
            "context_window_limit",
        ),
        (Some(200_000), None, 100, "request_byte_limit"),
    ] {
        let f = fixture(vec![]).await?;
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("trace.jsonl");
        let trace = JsonlTrace::open(TraceConfig {
            path: path.clone(),
            ..Default::default()
        })?;
        let mut captured = fork(f.registry.clone(), request());
        captured.context_window = window;
        captured.max_request_bytes = bytes;
        if let Some(window) = physical_window {
            captured.provider = Arc::new(PhysicalWindow {
                inner: f.registry.clone(),
                window,
            });
        }
        models(f.registry.clone(), Some(trace.clone()))?
            .summarize_fork(input(), captured)
            .await
            .unwrap_err();
        trace.flush(Duration::from_secs(2))?;
        let events = std::fs::read_to_string(path)?;
        assert!(events.contains("episode_fork_unavailable"));
        assert!(events.contains(reason));
        let rows = f.wire.requests.lock().unwrap();
        assert!(
            rows.is_empty(),
            "capacity failure must not issue a smaller episode-only call"
        );
    }
    Ok(())
}

struct PhysicalWindow {
    inner: Arc<dyn Provider>,
    window: usize,
}
#[async_trait]
impl Provider for PhysicalWindow {
    fn supports_summary_fork(&self, model: &str) -> bool {
        self.inner.supports_summary_fork(model)
    }
    fn summary_fork_mapping(&self, request: &ModelRequest) -> Result<Option<Value>> {
        self.inner.summary_fork_mapping(request)
    }
    fn estimate_input_tokens(
        &self,
        request: &ModelRequest,
        count: &dyn Fn(&Value) -> usize,
    ) -> Result<Option<usize>> {
        self.inner.estimate_input_tokens(request, count)
    }
    fn capabilities(&self, model: &str) -> bubblepaw_core::Capabilities {
        let mut caps = self.inner.capabilities(model);
        caps.context_window = Some(self.window);
        caps
    }
    async fn generate_summary_fork(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        self.inner
            .generate_summary_fork(request, cancellation)
            .await
    }
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        panic!("captured provider only admits isolated fork dispatch")
    }
}

struct UnavailableFree(Mutex<usize>);
#[async_trait]
impl Provider for UnavailableFree {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        *self.0.lock().unwrap() += 1;
        Err(ProviderError::new(ProviderErrorKind::Auth, "free route unavailable").into())
    }
}

#[tokio::test]
async fn successful_go_main_route_is_used_for_fork_without_changing_main_route_selection()
-> Result<()> {
    let f = fixture(vec![
        reply("warm Go"),
        reply("# Go episode\nObserved failure.\nOpen: fix"),
        reply("continue Go"),
    ])
    .await?;
    let free = Arc::new(UnavailableFree(Mutex::new(0)));
    let mut registry = ProviderRegistry::new("opencode-free", free.clone());
    registry.insert("opencode", f.registry.route("opencode/flash").1)?;
    registry.set_model_fallbacks(
        "opencode-free/flash-free".into(),
        vec!["opencode/flash".into()],
    )?;
    let registry = Arc::new(registry);
    let mut primary = request();
    primary.model = "opencode-free/flash-free".into();
    let warmed = registry
        .generate(primary.clone(), Arc::new(|_| {}), CancellationToken::new())
        .await?;
    assert_eq!(
        ProviderRegistry::selected_model(&warmed),
        Some("opencode/flash")
    );
    let mut captured = primary.clone();
    captured.model = ProviderRegistry::selected_model(&warmed).unwrap().into();
    models(registry.clone(), None)?
        .summarize_fork(input(), fork(registry.clone(), captured))
        .await?;
    let continued = registry
        .generate(primary, Arc::new(|_| {}), CancellationToken::new())
        .await?;
    assert_eq!(
        ProviderRegistry::selected_model(&continued),
        Some("opencode/flash")
    );
    assert_eq!(*free.0.lock().unwrap(), 1);
    let rows = f.wire.requests.lock().unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|(_, body)| body["model"] == "flash"));
    assert_eq!(
        rows[0].1["messages"],
        Value::Array(rows[1].1["messages"].as_array().unwrap()[..5].to_vec())
    );
    assert_eq!(rows[0].1, rows[2].1);
    Ok(())
}

#[tokio::test]
async fn invalid_free_reply_retries_paid_with_full_fork_shape_and_preserves_main_route()
-> Result<()> {
    let f = fixture(vec![reply("main warm"), reply("\n<｜DSML｜ calls>\n<｜DSML｜ invoke name=\"poll\">{\"handle\":\"process:pending\"}</｜DSML｜ invoke>\n</｜DSML｜ calls>"), reply("Failure remains [source-t]. Open: repair."), reply("main continues")]).await?;
    let provider = f.registry.route("opencode/flash").1;
    let mut registry = ProviderRegistry::new("opencode-free", provider.clone());
    registry.insert("opencode", provider)?;
    registry.set_model_fallbacks("opencode-free/flash".into(), vec!["opencode/flash".into()])?;
    let registry = Arc::new(registry);
    let mut original = request();
    original.model = "opencode-free/flash".into();
    let canonical_before = serde_json::to_value(&original)?;
    registry
        .generate(original.clone(), Arc::new(|_| {}), CancellationToken::new())
        .await?;
    let result = models(registry.clone(), None)?
        .summarize_fork(input(), fork(registry.clone(), original.clone()))
        .await?;
    assert_eq!(result.summary, "Failure remains [source-t]. Open: repair.");
    assert_eq!(
        (
            result.usage.input_tokens,
            result.usage.output_tokens,
            result.usage.cache_read_tokens
        ),
        (1002, 46, 800)
    );
    registry
        .generate(original.clone(), Arc::new(|_| {}), CancellationToken::new())
        .await?;
    assert_eq!(serde_json::to_value(original)?, canonical_before);
    let rows = f.wire.requests.lock().unwrap();
    assert_eq!(rows.len(), 4);
    let prefix = rows[0].1["messages"].as_array().unwrap();
    for index in [1, 2] {
        assert_eq!(
            &rows[index].1["messages"].as_array().unwrap()[..prefix.len()],
            prefix
        );
        assert_eq!(rows[index].1["tools"], rows[0].1["tools"]);
        assert_eq!(
            rows[index].1["reasoning_effort"],
            rows[0].1["reasoning_effort"]
        );
        assert!(!rows[index].1.to_string().contains("BOUNDED_INPUT_SENTINEL"));
    }
    assert_eq!(
        rows[1].1, rows[2].1,
        "retry preserves the complete compatible fork, not episode-only text"
    );
    assert_eq!(
        rows[0].1, rows[3].1,
        "summary fallback never changes main sticky selection or its prefix"
    );
    Ok(())
}

#[tokio::test]
async fn paid_route_recomputes_its_own_native_source_mapping() -> Result<()> {
    let f = fixture(vec![
        reply("<tool_call>invalid</tool_call>"),
        reply("Recorded [source-a]. Open: none."),
    ])
    .await?;
    let provider = f.registry.route("opencode/flash").1;
    let mut registry = ProviderRegistry::new("opencode-free", provider.clone());
    registry.insert("opencode", provider)?;
    registry.set_model_fallbacks(
        "opencode-free/flash".into(),
        vec!["opencode/paid-flash".into()],
    )?;
    let registry = Arc::new(registry);
    let mut original = request();
    original.model = "opencode-free/flash".into();
    models(registry.clone(), None)?
        .summarize_fork(input(), fork(registry, original))
        .await?;
    let rows = f.wire.requests.lock().unwrap();
    assert_eq!(rows.len(), 2);
    for (index, expected) in [
        (0, "\"wire_positions\":[2,3]"),
        (1, "\"wire_positions\":[2]"),
    ] {
        let messages = rows[index].1["messages"].as_array().unwrap();
        let control = messages[messages.len() - 2]["content"].as_str().unwrap();
        assert!(control.contains(expected), "{control}");
        assert!(rows[index].1.to_string().contains("MIDDLE_EXACT_FAILURE"));
        assert!(!rows[index].1.to_string().contains("BOUNDED_INPUT_SENTINEL"));
    }
    Ok(())
}

#[tokio::test]
async fn unsupported_fork_requires_explicit_episode_policy() -> Result<()> {
    let f = fixture(vec![reply("Explicit episode facts. Open: none.")]).await?;
    let mut captured = fork(f.registry.clone(), request());
    captured.provider = Arc::new(UnavailableFree(Mutex::new(0)));
    let adapter = models(f.registry.clone(), None)?;
    let error = adapter.summarize_fork(input(), captured).await.unwrap_err();
    assert!(error.to_string().contains("unsupported_protocol"));
    assert!(f.wire.requests.lock().unwrap().is_empty());
    let summary = adapter.summarize(input()).await?;
    assert_eq!(summary.summary, "Explicit episode facts. Open: none.");
    let rows = f.wire.requests.lock().unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].1.get("tools").is_none());
    assert!(rows[0].1.to_string().contains("BOUNDED_INPUT_SENTINEL"));
    Ok(())
}
