use super::*;
use crate::Tool;

struct AddedTool(&'static str);
#[async_trait]
impl Tool for AddedTool {
    fn definition(&self) -> crate::ToolDefinition {
        crate::ToolDefinition {
            name: self.0.into(),
            description: format!("Capability {}", self.0),
            input_schema: json!({"type":"object","additionalProperties":false}),
            read_only: true,
            approval: crate::Approval::None,
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<crate::ToolResult> {
        unreachable!()
    }
}
fn harness(store: Store, registry: Arc<Mutex<ToolRegistry>>) -> Harness {
    Harness::new(
        store,
        Arc::new(Answer),
        ToolRegistry::new(),
        HarnessConfig {
            registry_resolver: Some(Arc::new(move || registry.lock().unwrap().clone())),
            execution_mode: crate::ExecutionMode::Native,
            ..Default::default()
        },
    )
}
fn add(registry: &Mutex<ToolRegistry>, name: &'static str, generation: u64) -> Result<()> {
    let mut tools = registry.lock().unwrap();
    tools.register_host(Arc::new(AddedTool(name)))?;
    *tools = tools.clone().with_generation(generation);
    Ok(())
}
fn notices(store: &Store, session: &str) -> Result<Vec<Value>> {
    store
        .messages(session)?
        .iter()
        .filter(|message| {
            message
                .content
                .contains("<harness kind=\"tools_reloaded\">")
        })
        .map(|message| {
            Ok(serde_json::from_str(
                message
                    .content
                    .lines()
                    .find(|line| line.starts_with('{'))
                    .unwrap(),
            )?)
        })
        .collect()
}

#[tokio::test]
async fn undelivered_inventory_changes_survive_store_reopen_and_later_registry_updates()
-> Result<()> {
    for committed_b in [false, true] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("pending.db");
        let store = Store::open(&path)?;
        let session = store.create_session("pending inventory")?.id;
        let registry = Arc::new(Mutex::new(ToolRegistry::new()));
        add(&registry, "base_core", 1)?;
        let agent = harness(store.clone(), registry.clone());
        assert_eq!(
            agent
                .prompt(&session, "initial", RunOptions::default())
                .await?
                .status,
            "completed"
        );
        add(&registry, "added_b", 2)?;
        if committed_b {
            assert_eq!(
                agent
                    .prompt(&session, "deliver B", RunOptions::default())
                    .await?
                    .status,
                "completed"
            );
            assert_eq!(notices(&store, &session)?.len(), 1);
        } else {
            // Persist the observed inventory/pending notice, then stop before
            // ContextState can commit the ordinary reminder message.
            agent.refresh_run_tools(&session, &RunOptions::default(), &ToolRegistry::new())?;
            assert!(notices(&store, &session)?.is_empty());
            assert!(
                !store
                    .last_reminders(&session, "root")?
                    .iter()
                    .any(|reminder| reminder.kind == "tools_reloaded")
            );
        }
        let frozen = serde_json::to_vec(&agent.request_template(&session)?.tools)?;
        drop(agent);
        drop(store);
        // C retains B's addition and introduces another capability while offline.
        add(&registry, "added_c", 3)?;
        let store = Store::open(&path)?;
        let agent = harness(store.clone(), registry.clone());
        assert_eq!(
            agent
                .prompt(&session, "deliver latest inventory", RunOptions::default())
                .await?
                .status,
            "completed"
        );
        assert_eq!(
            serde_json::to_vec(&agent.request_template(&session)?.tools)?,
            frozen
        );
        let delivered = notices(&store, &session)?;
        assert_eq!(delivered.len(), if committed_b { 2 } else { 1 });
        let names = delivered.last().unwrap()["added"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            if committed_b {
                vec!["added_c"]
            } else {
                vec!["added_b", "added_c"]
            },
            "committed_b={committed_b}"
        );
        assert_eq!(
            agent
                .prompt(&session, "unchanged", RunOptions::default())
                .await?
                .status,
            "completed"
        );
        assert_eq!(
            notices(&store, &session)?,
            delivered,
            "committed notice is not redelivered"
        );
    }
    Ok(())
}
