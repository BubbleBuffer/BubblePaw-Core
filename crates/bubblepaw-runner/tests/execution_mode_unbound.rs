//! The mode-aware prompt resolver reads a chat's mode without binding it, so
//! introspection and the episode template leave a new chat unbound.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ExecutionMode, Harness, HarnessConfig, ModelRequest, ModelResponse,
    Provider, RunOptions, Store, TextCallback, ToolRegistry,
};
use bubblepaw_runner::execution_mode::configure_prompts;
use std::sync::Arc;

struct Unused;

#[async_trait]
impl Provider for Unused {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        unreachable!("reads make no model calls")
    }
}

#[test]
fn non_binding_reads_leave_the_chat_mode_unbound() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("unbound")?.id;
    let mut config = HarnessConfig {
        execution_mode: ExecutionMode::Native,
        ..Default::default()
    };
    configure_prompts(
        &mut config,
        store.clone(),
        "native prompt".into(),
        "js prompt".into(),
    );
    let harness = Harness::new(store.clone(), Arc::new(Unused), ToolRegistry::new(), config);
    let before = store.revision();
    harness.effective_tool_definitions(&session, &RunOptions::default())?;
    let template = harness.request_template(&session)?;
    assert!(template.system_prompt.starts_with("native prompt"));
    assert_eq!(store.revision(), before, "reads must not write");
    // Still unbound: the first binding takes whatever mode it is given.
    assert_eq!(
        store.resolve_execution_mode(&session, ExecutionMode::Js)?,
        ExecutionMode::Js
    );
    Ok(())
}
