//! Runner producers and restart recovery preserve exact identities without text markers.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ContextState, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider,
    RunOptions, Store, TextCallback, ToolRegistry,
};
use bubblepaw_runner::{
    agents::{AgentProfile, AgentTools},
    tools::{ProcessTools, WorkspaceTools},
};
use serde_json::json;
use std::sync::Arc;

struct Complete;
#[async_trait]
impl Provider for Complete {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(ModelResponse {
            text: "done".into(),
            ..Default::default()
        })
    }
}
fn profiles(harness: Arc<Harness>) -> Vec<AgentProfile> {
    vec![AgentProfile {
        name: "fixture".into(),
        description: "fixture".into(),
        harness,
        model: None,
        max_rounds: None,
        approval: None,
    }]
}

#[test]
fn restored_process_delivery_uses_handle_metadata_when_text_has_no_handle() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let owner = store.create_session("process metadata")?.id;
    let handle = "process:synthetic-completed";
    store.store_attachment(
        &owner,
        &format!("bubblepaw-process-notice-v1:{handle}"),
        &json!({"handle":handle,"notice":"A known terminal outcome; no handle appears here."})
            .to_string(),
        b"",
    )?;
    let processes = ProcessTools::new(WorkspaceTools::new(temp.path())?);
    processes.set_store(store.clone());
    let mut reminders = processes.take_reminders(&owner);
    assert_eq!(reminders.len(), 1);
    assert_eq!(reminders[0].deliveries[0].kind, "process_completion");
    assert_eq!(reminders[0].deliveries[0].id, handle);
    assert!(!reminders[0].text.contains(handle));
    let state = ContextState::default();
    state.remind_with_metadata(&owner, "root", reminders.remove(0));
    state.build_persisted(ModelRequest::default(), &owner, "root", &[], &store, |_| {
        Ok(())
    })?;
    let restored = ProcessTools::new(WorkspaceTools::new(temp.path())?);
    restored.set_store(store.clone());
    assert!(restored.take_reminders(&owner).is_empty());
    assert!(restored.take_reminders(&owner).is_empty());
    assert_eq!(store.messages(&owner)?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn restored_child_delivery_uses_exact_session_and_run_metadata() -> Result<()> {
    let store = Store::in_memory()?;
    let owner = store.create_session("child metadata")?.id;
    let child = store
        .create_session_with_parent("visible child", Default::default(), Some(&owner))?
        .id;
    let harness = Arc::new(Harness::new(
        store.clone(),
        Arc::new(Complete),
        ToolRegistry::new(),
        HarnessConfig::default(),
    ));
    let run = harness
        .prompt(&child, "synthetic task", RunOptions::default())
        .await?;
    let agents = AgentTools::from_profiles(store.clone(), profiles(harness.clone()))?;
    let mut reminders = agents.take_reminders(&owner);
    assert_eq!(reminders.len(), 1);
    assert_eq!(reminders[0].deliveries[0].kind, "child_completion");
    assert_eq!(
        reminders[0].deliveries[0].id,
        serde_json::to_string(&[child.as_str(), run.id.as_str()])?
    );
    reminders[0].text = "Known child completion; no session or run marker appears here.".into();
    let state = ContextState::default();
    state.remind_with_metadata(&owner, "root", reminders.remove(0));
    state.build_persisted(ModelRequest::default(), &owner, "root", &[], &store, |_| {
        Ok(())
    })?;
    let restored = AgentTools::from_profiles(store.clone(), profiles(harness.clone()))?;
    assert!(restored.take_reminders(&owner).is_empty());
    assert!(restored.take_reminders(&owner).is_empty());
    assert_eq!(store.messages(&owner)?.len(), 1);
    harness.shutdown(std::time::Duration::from_secs(2)).await?;
    Ok(())
}
