//! Registry snapshot ordering and explicit JavaScript refresh boundaries.
use super::*;

/// A small JavaScript-style adapter: only `js` is declared; host calls go
/// through the dispatcher and their definitions become its generated help.
struct JsBridge;
#[async_trait]
impl Tool for JsBridge {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "js".into(),
            description: "Compose host calls".into(),
            input_schema: serde_json::json!({"type":"object","properties":{"target":{"type":"string"}},"required":["target"],"additionalProperties":false}),
            read_only: true,
            approval: Approval::None,
        }
    }
    fn definition_in(&self, registry: &ToolRegistry) -> ToolDefinition {
        let mut definition = self.definition();
        for tool in registry.capabilities() {
            definition
                .description
                .push_str(&format!("\n- {}: {}", tool.name, tool.description));
        }
        definition
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        unreachable!()
    }
    async fn invoke_with_dispatcher(
        &self,
        context: ToolContext,
        input: Value,
        dispatcher: ToolDispatcher,
    ) -> Result<ToolResult> {
        dispatcher
            .invoke(
                context,
                input["target"].as_str().unwrap(),
                serde_json::json!({}),
            )
            .await
    }
}
fn js_call(id: &str, target: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "js".into(),
        arguments: serde_json::json!({"target":target}),
    }
}

struct Swap {
    live: Arc<Mutex<ToolRegistry>>,
    next: ToolRegistry,
    refresh: bool,
}
#[async_trait]
impl Tool for Swap {
    fn definition(&self) -> ToolDefinition {
        value("swap", "replace host snapshot").definition()
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        unreachable!()
    }
    async fn invoke_with_dispatcher(
        &self,
        _: ToolContext,
        _: Value,
        dispatcher: ToolDispatcher,
    ) -> Result<ToolResult> {
        *self.live.lock().unwrap() = self.next.clone();
        if self.refresh {
            dispatcher.refresh_tools_next_round();
        }
        Ok(ToolResult::text("swapped"))
    }
}
struct StartSwap {
    live: Arc<Mutex<ToolRegistry>>,
    next: ToolRegistry,
    calls: AtomicUsize,
}
#[async_trait]
impl TurnStart for StartSwap {
    async fn before_turn(&self, _: &str, _: CancellationToken) -> Result<()> {
        tokio::task::yield_now().await;
        *self.live.lock().unwrap() = self.next.clone();
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn turn_start_refreshes_live_calls_without_rebinding_the_initial_prefix() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("turn start")?;
    let live = Arc::new(Mutex::new(ToolRegistry::new()));
    let new_tool = value("new", "new tool");
    let mut next = ToolRegistry::new();
    next.register(new_tool.clone())?;
    let hook = Arc::new(StartSwap {
        live: live.clone(),
        next,
        calls: AtomicUsize::default(),
    });
    let source = live.clone();
    let invoke = |id: &str| {
        calls(vec![ToolCall {
            id: id.into(),
            name: "call_tool".into(),
            arguments: serde_json::json!({"name":"new","arguments":{}}),
        }])
    };
    let provider = Script::new(vec![
        invoke("new-first"),
        done(),
        invoke("new-second"),
        done(),
    ]);
    let harness = Harness::new(
        store,
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig {
            registry_resolver: Some(Arc::new(move || source.lock().unwrap().clone())),
            turn_start: Some(hook.clone()),
            ..Default::default()
        },
    );
    let initial = harness.request_template(&session.id)?;
    for _ in 0..2 {
        let run = harness
            .prompt(&session.id, "go", RunOptions::default())
            .await?;
        assert_eq!(run.status, "completed", "{:?}", run.error);
    }
    assert_eq!(hook.calls.load(Ordering::SeqCst), 2);
    assert_eq!(new_tool.calls.load(Ordering::SeqCst), 2);
    for request in provider.requests.lock().unwrap().iter() {
        assert_eq!(
            serde_json::to_value(&request.tools)?,
            serde_json::to_value(&initial.tools)?
        );
        assert_eq!(request.system_prompt, initial.system_prompt);
        assert!(!request.tools.iter().any(|tool| tool.name == "new"));
        assert!(request.tools.iter().any(|tool| tool.name == "call_tool"));
        assert!(
            request
                .messages
                .iter()
                .any(|message| message.content.contains("tools_reloaded")
                    && message.content.contains("new tool"))
        );
    }
    Ok(())
}

#[tokio::test]
async fn registry_swaps_refresh_only_after_own_request_and_keep_call_budget() -> Result<()> {
    for refresh in [false, true] {
        let store = Store::in_memory()?;
        let session = store.create_session("snapshot boundary")?;
        let live = Arc::new(Mutex::new(ToolRegistry::new()));
        let old = value("value", "old");
        let new = value("value", "new");
        let mut next = ToolRegistry::new();
        next.register(Arc::new(JsBridge))?;
        next.register_host(new.clone())?;
        // A newly added entrypoint must still respect the run's selection.
        let hidden = value("hidden", "must remain hidden");
        next.register_host(hidden.clone())?;
        let mut initial = ToolRegistry::new();
        initial.register(Arc::new(JsBridge))?;
        initial.register_host(old.clone())?;
        initial.register_host(Arc::new(Swap {
            live: live.clone(),
            next,
            refresh,
        }))?;
        *live.lock().unwrap() = initial;
        let source = live.clone();
        let provider = Script::new(vec![
            calls(vec![js_call("swap", "swap"), js_call("old", "value")]),
            calls(vec![js_call("next", "value")]),
            calls(vec![js_call("over-budget", "value")]),
            done(),
            calls(vec![js_call("new-turn-hidden", "hidden")]),
            done(),
        ]);
        let harness = Harness::new(
            store.clone(),
            provider.clone(),
            ToolRegistry::new(),
            HarnessConfig {
                execution_mode: ExecutionMode::Js,
                registry_resolver: Some(Arc::new(move || source.lock().unwrap().clone())),
                ..Default::default()
            },
        );
        let run = harness
            .prompt(
                &session.id,
                "go",
                RunOptions {
                    // Three allowed wrapper+host pairs share one six-call budget.
                    max_tool_calls: Some(6),
                    tool_names: Some(vec!["swap".into(), "value".into()]),
                    ..Default::default()
                },
            )
            .await?;
        assert_eq!(run.status, "completed");
        assert_eq!(
            old.calls.load(Ordering::SeqCst),
            if refresh { 1 } else { 2 }
        );
        assert_eq!(new.calls.load(Ordering::SeqCst), usize::from(refresh));
        let history = store.messages(&session.id)?;
        assert!(history.iter().any(|message| message.tool_call_id.as_deref()
            == Some("over-budget")
            && message.content.contains("Tool call budget exhausted")));
        {
            let requests = provider.requests.lock().unwrap();
            let round_one = &requests[1].tools;
            assert_eq!(round_one.len(), 1);
            assert_eq!(round_one[0].name, "js");
            assert_eq!(
                serde_json::to_vec(round_one)?,
                serde_json::to_vec(&requests[0].tools)?
            );
            assert!(round_one[0].description.contains("- value: old"));
            assert!(!round_one[0].description.contains("- value: new"));
            assert!(
                !round_one[0].description.contains("hidden"),
                "the initial adapter help stays fixed after a live inventory refresh"
            );
        }
        // The next run gets live capabilities and a fresh allowlist, while its
        // declared JS help remains the chat's initial byte-identical snapshot.
        harness
            .prompt(&session.id, "again", RunOptions::default())
            .await?;
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 6);
        for request in &requests[4..] {
            assert_eq!(
                serde_json::to_vec(&request.tools)?,
                serde_json::to_vec(&requests[0].tools)?
            );
            assert_eq!(request.system_prompt, requests[0].system_prompt);
            assert!(!request.tools[0].description.contains("hidden"));
            assert!(request.tools[0].description.contains("- value: old"));
        }
        assert_eq!(
            hidden.calls.load(Ordering::SeqCst),
            1,
            "the current run can invoke the newly allowed tool despite frozen help"
        );
    }
    Ok(())
}
