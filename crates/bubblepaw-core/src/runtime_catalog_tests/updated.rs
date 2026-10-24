use super::*;
use crate::Tool;

struct VersionedTool(&'static str);
#[async_trait]
impl Tool for VersionedTool {
    fn definition(&self) -> crate::ToolDefinition {
        crate::ToolDefinition {
            name: "existing_core".into(),
            description: format!("Core definition {}", self.0),
            input_schema: json!({"type":"object","properties":{(self.0):{"type":"string"}},"required":[self.0],"additionalProperties":false}),
            read_only: true,
            approval: crate::Approval::None,
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<crate::ToolResult> {
        unreachable!()
    }
}

#[tokio::test]
async fn applied_catalog_retains_updated_definition_of_an_initial_core_tool() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("updated initial core schema")?.id;
    let mut initial = ToolRegistry::new();
    initial.register_host(Arc::new(VersionedTool("old_field")))?;
    let registry = Arc::new(Mutex::new(initial.with_generation(1)));
    let resolver = registry.clone();
    let hook = Arc::new(ColdHook {
        view: Store::message(&session, Role::System, "Applied memory view".into()),
        note: Mutex::new("Fixed view instructions".into()),
    });
    let harness = Harness::new(
        store.clone(),
        Arc::new(Answer),
        ToolRegistry::new(),
        HarnessConfig {
            registry_resolver: Some(Arc::new(move || resolver.lock().unwrap().clone())),
            execution_mode: crate::ExecutionMode::Native,
            context: hook.clone(),
            ..Default::default()
        },
    );
    assert_eq!(
        harness
            .prompt(&session, "initial", RunOptions::default())
            .await?
            .status,
        "completed"
    );
    let first = harness.request_template(&session)?;
    let old = VersionedTool("old_field").definition();
    assert_eq!(
        serde_json::to_value(
            first
                .tools
                .iter()
                .find(|tool| tool.name == old.name)
                .unwrap()
        )?,
        serde_json::to_value(&old)?
    );
    {
        let mut tools = registry.lock().unwrap();
        tools.remove("existing_core", "definition changed");
        tools.register_host(Arc::new(VersionedTool("new_field")))?;
        *tools = tools.clone().with_generation(2);
    }
    assert_eq!(
        harness
            .prompt(&session, "observe new schema", RunOptions::default())
            .await?
            .status,
        "completed"
    );
    let live = harness.selected_tools(&session, &RunOptions::default())?;
    // Match the runtime apply path: declaring the new view admits replacement
    // cold notes before the current catalog is published and staged.
    harness.declare_context_change(&session, "lcm_apply");
    harness.update_applied_catalog(&session, &live)?;
    let surface = store.chat_tool_surface(&session)?.unwrap();
    let catalog: Value = serde_json::from_str(surface.cold_catalog.as_ref().unwrap())?;
    let current = VersionedTool("new_field").definition();
    assert_eq!(catalog["updated"], json!([current]));
    assert_eq!(catalog["added"], json!([]));
    assert_eq!(catalog["removed"], json!([]));
    let mut request = harness.request_template(&session)?;
    assert_eq!(
        serde_json::to_vec(&request.tools)?,
        serde_json::to_vec(&first.tools)?
    );
    assert_eq!(request.system_prompt, first.system_prompt);
    // Recreate an ordinary request from the hook's actual projected cold view
    // and canonical history. Preparation clears/restages the persisted notes.
    request.messages = hook.project_history(&store.messages(&session)?)?;
    let prepared = harness
        .prepare_with_writer(
            &session,
            request,
            Arc::new(Answer),
            crate::PrepareMode::Request,
            CancellationToken::new(),
        )
        .await?;
    let rendered =
        harness.build_context_request(prepared.request, &session, None, Arc::new(Answer))?;
    let encoded = rendered
        .messages
        .iter()
        .find_map(|message| {
            message
                .content
                .split_once("<harness kind=\"tool_catalog\">")?
                .1
                .split_once("</harness>")
                .map(|(body, _)| body)
        })
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(encoded)?["updated"],
        json!([VersionedTool("new_field").definition()])
    );
    // The catalog is staged through the same writer as the hook's notes, after them.
    let cold = rendered
        .messages
        .iter()
        .find(|message| message.content.contains("<harness kind=\"tool_catalog\">"))
        .unwrap();
    let host = cold.content.find("<harness kind=\"host_note\">").unwrap();
    let catalog = cold
        .content
        .find("<harness kind=\"tool_catalog\">")
        .unwrap();
    assert!(host < catalog, "tool_catalog follows the hook's cold notes");
    Ok(())
}
