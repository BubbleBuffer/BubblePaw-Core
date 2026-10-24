use super::*;

#[tokio::test]
async fn completion_survives_exhaustion_while_ordinary_calls_keep_the_shared_budget() -> Result<()>
{
    let f = fixture(vec![batch(vec![
        call("effect", json!({})),
        call("effect", json!({})),
        call("submit_result", json!({"answer":8})),
    ])])?;
    let mut options = options();
    options.max_tool_calls = Some(1);
    let run = f.harness.prompt(&f.session, "compute", options).await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(run.text.as_deref(), Some(r#"{"answer":8}"#));
    assert_eq!(f.effect.0.load(Ordering::SeqCst), 1);
    assert!(
        f.store
            .messages(&f.session)?
            .iter()
            .any(|message| message.role == Role::Tool
                && message.is_error
                && message.content.contains("budget exhausted"))
    );
    Ok(())
}

#[tokio::test]
async fn completion_through_call_tool_still_works_at_zero_budget() -> Result<()> {
    let f = fixture(vec![batch(vec![call(
        "call_tool",
        json!({"name":"submit_result","arguments":{"answer":4}}),
    )])])?;
    let mut options = options();
    options.max_tool_calls = Some(0);
    let run = f.harness.prompt(&f.session, "compute", options).await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    Ok(())
}

#[tokio::test]
async fn approval_denial_still_applies_to_terminal_submission() -> Result<()> {
    let f = fixture(vec![
        submit(json!({"answer":1})),
        text("plain"),
        text("again"),
    ])?;
    let mut options = options();
    let mut approval = ApprovalPolicy::default();
    approval
        .tools
        .insert("submit_result".into(), ApprovalMode::Deny);
    options.approval = Some(approval);
    let run = f.harness.prompt(&f.session, "compute", options).await?;
    assert_eq!(run.status, "failed");
    assert!(
        f.store
            .messages(&f.session)?
            .iter()
            .any(|message| message.role == Role::Tool
                && message.is_error
                && message.content.to_lowercase().contains("denied"))
    );
    Ok(())
}

#[tokio::test]
async fn result_slots_do_not_leak_between_runs_or_sessions() -> Result<()> {
    let f = fixture(vec![
        submit(json!({"answer":1})),
        submit(json!({"answer":2})),
        submit(json!({"answer":3})),
    ])?;
    let second_session = f.store.create_session("other session")?.id;
    for (session, answer) in [(&f.session, 1), (&second_session, 2), (&f.session, 3)] {
        let run = f.harness.prompt(session, "compute", options()).await?;
        assert_eq!(run.status, "completed", "{:?}", run.error);
        assert_eq!(
            serde_json::from_str::<Value>(run.text.as_deref().unwrap())?,
            json!({"answer":answer})
        );
    }
    Ok(())
}

struct ApplyContext {
    prepares: AtomicUsize,
    applied: std::sync::atomic::AtomicBool,
}
#[async_trait]
impl ContextPreparation for ApplyContext {
    async fn prepare(&self, call: PrepareCall<'_>) -> Result<PreparedContext> {
        self.prepares.fetch_add(1, Ordering::SeqCst);
        let mut request = call.request;
        // A disposable host projection cannot replace the run's completion contract.
        request.tools.clear();
        request.output_schema = Some(JsonSchema::new("wrong", json!({"type":"string"})).unwrap());
        Ok(PreparedContext {
            request,
            notice: None,
            usage: None,
        })
    }
    fn take_prefix_change(&self, _: &str) -> Option<String> {
        (self.prepares.load(Ordering::SeqCst) >= 2 && !self.applied.swap(true, Ordering::SeqCst))
            .then(|| "lcm_apply".into())
    }
}

#[tokio::test]
async fn newly_required_result_survives_projection_reload_and_apply_with_the_original_chat_tools()
-> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("apply")?.id;
    let provider = Arc::new(Scripted {
        replies: Mutex::new(
            vec![
                text("initial ordinary run"),
                submit(json!({"answer":"invalid"})),
                submit(json!({"answer":6})),
            ]
            .into(),
        ),
        ..Default::default()
    });
    let generation = Arc::new(AtomicUsize::new(1));
    let resolver = {
        let generation = generation.clone();
        Arc::new(move || {
            ToolRegistry::new().with_generation(generation.fetch_add(1, Ordering::SeqCst) as u64)
        }) as ToolRegistryResolver
    };
    let harness = Harness::new(
        store,
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig {
            context: Arc::new(ApplyContext {
                prepares: AtomicUsize::new(0),
                applied: std::sync::atomic::AtomicBool::new(false),
            }),
            registry_resolver: Some(resolver),
            ..Default::default()
        },
    );
    let initial = harness
        .prompt(&session, "warm", RunOptions::default())
        .await?;
    assert_eq!(initial.status, "completed", "{:?}", initial.error);
    let run = harness.prompt(&session, "compute", options()).await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(run.text.as_deref(), Some(r#"{"answer":6}"#));
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    for request in requests.iter() {
        assert_eq!(
            serde_json::to_vec(&request.tools)?,
            serde_json::to_vec(&requests[0].tools)?
        );
        assert_eq!(request.system_prompt, requests[0].system_prompt);
        assert!(
            !request
                .tools
                .iter()
                .any(|tool| tool.name == "submit_result")
        );
        assert!(request.output_schema.is_none());
    }
    let marker = format!("<harness kind=\"result_schema:{}\">", run.id);
    let current = requests[1]
        .messages
        .iter()
        .find_map(|message| {
            message
                .content
                .split_once(&marker)?
                .1
                .split_once("</harness>")
                .map(|(body, _)| body)
        })
        .expect("the current result contract is supplied in a run-start notice");
    let definition = current
        .lines()
        .find_map(|line| serde_json::from_str::<ToolDefinition>(line).ok())
        .expect("notice contains a complete current tool definition");
    assert_eq!(definition.name, "submit_result");
    assert_eq!(
        definition.input_schema,
        options().output_schema.unwrap().schema
    );
    Ok(())
}

#[derive(Default)]
struct SteeringText {
    requests: Mutex<Vec<ModelRequest>>,
    entered: tokio::sync::Notify,
    resume: tokio::sync::Notify,
    complete: bool,
}
#[async_trait]
impl Provider for SteeringText {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let count = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request);
            requests.len()
        };
        if count > 2 {
            return Err(anyhow!(
                "Missing-result enforcement was bypassed by steering"
            ));
        }
        if !self.complete || count == 1 {
            self.entered.notify_one();
            self.resume.notified().await;
        }
        Ok(if self.complete {
            submit(json!({"answer":count}))
        } else {
            text("text without submission")
        })
    }
}

#[tokio::test]
async fn text_only_responses_with_pending_steering_still_get_exactly_one_nudge() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("steered required result")?.id;
    let provider = Arc::new(SteeringText::default());
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    let task = tokio::spawn({
        let harness = harness.clone();
        let session = session.clone();
        async move { harness.prompt(&session, "compute", options()).await }
    });
    for index in 0..2 {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            provider.entered.notified(),
        )
        .await?;
        harness.steer(&session, format!("steering {index}"))?;
        provider.resume.notify_one();
    }
    let run = tokio::time::timeout(std::time::Duration::from_secs(5), task).await???;
    assert_eq!(run.status, "failed");
    assert!(
        run.error
            .unwrap()
            .contains("submit_result was not called after one host reminder")
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.content == "steering 0")
    );
    assert_eq!(
        store
            .messages(&session)?
            .iter()
            .filter(|message| message.content.contains("<harness kind=\"result_required:"))
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn steering_during_submission_is_serviced_in_a_fresh_run() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("steered completion")?.id;
    let provider = Arc::new(SteeringText {
        complete: true,
        ..Default::default()
    });
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    let task = tokio::spawn({
        let harness = harness.clone();
        let session = session.clone();
        async move {
            let mut options = options();
            options.max_tool_calls = Some(1);
            harness.prompt(&session, "compute", options).await
        }
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        provider.entered.notified(),
    )
    .await?;
    harness.steer(&session, "follow up after completion")?;
    provider.resume.notify_one();
    let run = tokio::time::timeout(std::time::Duration::from_secs(5), task).await???;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(run.text.as_deref(), Some(r#"{"answer":2}"#));
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.content == "follow up after completion")
    );
    let history = store.messages(&session)?;
    let budget_starts = history
        .iter()
        .filter(|message| {
            message
                .content
                .contains("You have 1 tool calls for this run")
        })
        .count();
    assert_eq!(
        budget_starts, 2,
        "the queued input receives a fresh run budget"
    );
    let finals: Vec<_> = history
        .iter()
        .filter(|message| {
            message.role == Role::Assistant && message.origin == Some(MessageOrigin::Generated)
        })
        .collect();
    assert_eq!(finals.len(), 2);
    assert_eq!(finals[0].content, r#"{"answer":1}"#);
    assert_eq!(finals[1].content, r#"{"answer":2}"#);
    Ok(())
}

#[tokio::test]
async fn repeated_required_result_runs_each_receive_a_fresh_nudge() -> Result<()> {
    let f = fixture(vec![
        text("first text"),
        submit(json!({"answer":1})),
        text("second text"),
        submit(json!({"answer":2})),
    ])?;
    let mut runs = Vec::new();
    for answer in 1..=2 {
        let run = f.harness.prompt(&f.session, "compute", options()).await?;
        assert_eq!(run.status, "completed", "{:?}", run.error);
        assert_eq!(
            serde_json::from_str::<Value>(run.text.as_deref().unwrap())?,
            json!({"answer":answer})
        );
        runs.push(run);
    }
    let requests = f.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    for (run, request) in runs.iter().zip([&requests[1], &requests[3]]) {
        let kind = format!("<harness kind=\"result_required:{}\"", run.id);
        assert!(request.messages.last().unwrap().content.contains(&kind));
    }
    assert_eq!(
        f.store
            .messages(&f.session)?
            .iter()
            .filter(|message| message.content.contains("<harness kind=\"result_required:"))
            .count(),
        2
    );
    Ok(())
}
