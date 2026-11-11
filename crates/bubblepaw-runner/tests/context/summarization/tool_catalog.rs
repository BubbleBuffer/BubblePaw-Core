//! Real runner LCM apply with deterministic summaries and a live host registry.
use super::*;
use bubblepaw_core::{
    ExecutionMode, Harness, HarnessConfig, RunOptions, Tool, ToolContext, ToolDefinition,
    ToolRegistry, ToolResult,
};
use serde_json::Value;
use std::sync::RwLock;

struct InventoryTool(&'static str);
#[async_trait]
impl Tool for InventoryTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.0.into(),
            description: format!("Inspect {} records", self.0),
            input_schema: if self.0.starts_with("mcp__") {
                json!({"type":"object","properties":{"private_deferred_field":{"type":"string","description":"DEFERRED_SCHEMA_ONLY"}},"additionalProperties":false})
            } else {
                json!({"type":"object","properties":{"query":{"type":"string"},"options":{"type":"object","properties":{"limit":{"type":"integer","minimum":1,"maximum":30}},"additionalProperties":false}},"required":["query"],"additionalProperties":false})
            },
            read_only: true,
            approval: Default::default(),
        }
    }
    async fn invoke(&self, _: ToolContext, input: Value) -> Result<ToolResult> {
        Ok(ToolResult::text(input.to_string()))
    }
}
fn add_deferred(tools: &mut ToolRegistry, name: &'static str, server: &str) -> Result<()> {
    tools.register_host(Arc::new(InventoryTool(name)))?;
    tools.set_deferred(name, true);
    tools.set_tool_server(name, server);
    Ok(())
}
fn harness_block<'a>(request: &'a ModelRequest, kind: &str) -> Option<(&'a Message, &'a str)> {
    let marker = format!("<harness kind=\"{kind}\">");
    request.messages.iter().find_map(|message| {
        let (_, tail) = message.content.split_once(&marker)?;
        Some((message, tail.split_once("</harness>")?.0))
    })
}
fn frozen(request: &ModelRequest) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(
        &json!({"system":request.system_prompt,"tools":request.tools}),
    )?)
}

#[tokio::test]
async fn real_lcm_apply_preserves_frozen_chat_prefix_and_refreshes_cold_tool_catalog() -> Result<()>
{
    let f = Fixture::new()?;
    let (policy, worker, models) = f.worker(60_000)?;
    let provider = Arc::new(RecordingProvider::default());
    let mut initial = ToolRegistry::new();
    initial.register_host(Arc::new(InventoryTool("keep_core")))?;
    initial.register_host(Arc::new(InventoryTool("former_core")))?;
    add_deferred(&mut initial, "mcp__old__lookup", "old.server")?;
    let inventory = Arc::new(RwLock::new(initial.with_generation(1)));
    let resolver = inventory.clone();
    let harness = Harness::new(
        f.store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig {
            model: "fake".into(),
            system_prompt: "Fixed host instructions".into(),
            execution_mode: ExecutionMode::Native,
            registry_resolver: Some(Arc::new(move || resolver.read().unwrap().clone())),
            context: policy,
            ..Default::default()
        },
    );
    assert_eq!(
        harness
            .prompt(&f.session, "First typed turn", RunOptions::default())
            .await?
            .status,
        "completed"
    );
    let first = provider.requests.lock().unwrap()[0].clone();
    assert!(first.tools.iter().any(|tool| tool.name == "former_core"));
    assert!(!first.tools.iter().any(|tool| tool.name == "new_core"));
    assert!(first.system_prompt.contains("old.server"));
    {
        let mut tools = inventory.write().unwrap();
        tools.remove("former_core", "removed by reload");
        tools.remove("mcp__old__lookup", "server disconnected");
        tools.register_host(Arc::new(InventoryTool("new_core")))?;
        add_deferred(&mut tools, "mcp__late__lookup", "late.server")?;
        *tools = tools.clone().with_generation(2);
    }
    assert_eq!(
        harness
            .prompt(
                &f.session,
                "Observe the changed inventory",
                RunOptions::default()
            )
            .await?
            .status,
        "completed"
    );
    let second = provider.requests.lock().unwrap()[1].clone();
    assert_eq!(frozen(&second)?, frozen(&first)?);
    let (_, notice) = harness_block(&second, "tools_reloaded")
        .expect("changed live registry stages a durable notice");
    let encoded = notice
        .lines()
        .find(|line| line.starts_with('{'))
        .expect("inventory notice contains compact JSON");
    let changed: Value = serde_json::from_str(encoded)?;
    assert!(
        changed["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "new_core")
    );
    assert!(
        changed["removed"]
            .as_array()
            .unwrap()
            .contains(&json!("former_core"))
    );
    assert!(!notice.contains("DEFERRED_SCHEMA_ONLY"));
    // Put the notice strictly before the newest typed turn retained by manual LCM.
    assert_eq!(
        harness
            .prompt(
                &f.session,
                "Retain this newest typed turn",
                RunOptions::default()
            )
            .await?
            .status,
        "completed"
    );
    let before = f.store.messages(&f.session)?;
    let notices = before
        .iter()
        .filter(|message| {
            message
                .content
                .contains("<harness kind=\"tools_reloaded\">")
        })
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(notices.len(), 1);
    assert!(f.store.context_view(&f.session)?.is_none());
    let main_calls = provider.requests.lock().unwrap().len();
    let compacted = harness.compact_session(&f.session, None).await?;
    assert!(compacted.is_some());
    worker.wait_idle(&f.session).await;
    assert!(
        models.calls.load(Ordering::SeqCst) > 0,
        "real EpisodeWorker produced memory"
    );
    assert!(f.store.context_view(&f.session)?.is_some());
    assert_eq!(
        provider.requests.lock().unwrap().len(),
        main_calls,
        "compaction uses episode models, not the main provider"
    );
    assert_eq!(
        harness
            .prompt(
                &f.session,
                "Fourth typed turn after compaction",
                RunOptions::default()
            )
            .await?
            .status,
        "completed"
    );
    let fourth = provider.requests.lock().unwrap()[3].clone();
    assert_eq!(frozen(&fourth)?, frozen(&first)?);
    assert_ne!(
        fourth.history.epoch, first.history.epoch,
        "LCM applied a new serving view"
    );
    let (cold, encoded) = harness_block(&fourth, "tool_catalog")
        .expect("applied inventory is present in cold context");
    assert_eq!(cold.role, Role::System);
    let catalog: Value = serde_json::from_str(encoded)?;
    assert_eq!(serde_json::to_string(&catalog)?, encoded);
    let anchors = catalog["anchors"].as_str().unwrap();
    assert!(anchors.contains("late.server") && anchors.contains("mcp__late__lookup"));
    assert!(!anchors.contains("old.server"));
    let added = catalog["added"].as_array().unwrap();
    let core = added
        .iter()
        .find(|tool| tool["name"] == "new_core")
        .unwrap();
    assert_eq!(
        *core,
        serde_json::to_value(InventoryTool("new_core").definition())?
    );
    let deferred = added
        .iter()
        .find(|tool| tool["name"] == "mcp__late__lookup")
        .unwrap();
    assert_eq!(
        *deferred,
        json!({"name":"mcp__late__lookup","server":"late.server","deferred":true})
    );
    assert!(
        !encoded.contains("private_deferred_field") && !encoded.contains("DEFERRED_SCHEMA_ONLY")
    );
    assert!(
        catalog["removed"]
            .as_array()
            .unwrap()
            .contains(&json!("former_core"))
    );
    assert!(
        harness_block(&fourth, "tools_reloaded").is_none(),
        "old notices were summarized out of the serving view"
    );
    assert!(
        !fourth
            .messages
            .iter()
            .any(|message| message.content.contains("Tool inventory changed"))
    );
    assert!(
        fourth
            .messages
            .iter()
            .any(|message| message.content == "Retain this newest typed turn")
    );
    let canonical = f.store.messages(&f.session)?;
    for notice in notices {
        assert!(
            canonical.contains(&notice),
            "canonical notice is unchanged after apply"
        );
    }
    assert!(
        canonical.starts_with(&before),
        "LCM retained the complete original journal"
    );
    assert!(fourth.cache_tiers.unwrap().cold_end.is_some());
    let next = f.store.create_session("new chat after registry update")?;
    assert_eq!(
        harness
            .prompt(&next.id, "New chat", RunOptions::default())
            .await?
            .status,
        "completed"
    );
    let requests = provider.requests.lock().unwrap();
    let fresh = requests.last().unwrap();
    assert!(fresh.tools.iter().any(|tool| tool.name == "new_core"));
    assert!(!fresh.tools.iter().any(|tool| tool.name == "former_core"));
    assert!(
        fresh.system_prompt.contains("late.server") && !fresh.system_prompt.contains("old.server")
    );
    Ok(())
}
