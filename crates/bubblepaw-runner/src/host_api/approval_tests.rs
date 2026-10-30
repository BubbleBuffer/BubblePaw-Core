use super::*;
use anyhow::Result;
use bubblepaw_core::{EventKind, ToolCall, ToolRegistry};

struct ApprovalWrite(std::path::PathBuf);
#[async_trait::async_trait]
impl bubblepaw_core::Tool for ApprovalWrite {
    fn definition(&self) -> bubblepaw_core::ToolDefinition {
        bubblepaw_core::ToolDefinition {
            name: "approval_write".into(),
            description: "Fixture requiring an explicit decision".into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
            read_only: false,
            approval: bubblepaw_core::Approval::Confirm,
        }
    }
    async fn invoke(
        &self,
        _context: bubblepaw_core::ToolContext,
        _input: Value,
    ) -> Result<bubblepaw_core::ToolResult> {
        std::fs::write(&self.0, "must not be written")?;
        Ok(bubblepaw_core::ToolResult::text("wrote fixture"))
    }
}

#[tokio::test]
async fn edit_profile_can_answer_an_approval_and_denial_prevents_the_write() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let session = store.create_session_with_workspace("approval", root.path())?;
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ApprovalWrite(root.path().join("unapproved.txt"))))?;
    let provider = crate::provider::ScriptedProvider::with_steps(
        vec![crate::provider::ScriptedStep::Calls {
            calls: vec![ToolCall {
                id: "write-fixture".into(),
                name: "approval_write".into(),
                arguments: json!({}),
            }],
        }],
        "denial handled",
    );
    let harness = Arc::new(Harness::new(
        store.clone(),
        Arc::new(provider),
        registry,
        Default::default(),
    ));
    let ctx = HostCtx::new(harness, store, root.path());
    let mut events = events::subscribe(&ctx, None, Some(&session.id))?.receiver;
    dispatch(
        &ctx,
        Profile::Edit,
        "run.start",
        json!({"session_id":session.id,"text":"request a write"}),
    )
    .await?;
    let approval = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let EventKind::ApprovalRequested { approval } = events.recv().await?.kind {
                return Ok::<_, anyhow::Error>(approval);
            }
        }
    })
    .await??;
    let pending = dispatch(
        &ctx,
        Profile::Read,
        "approval.list",
        json!({"session_id":session.id}),
    )
    .await?;
    assert_eq!(pending[0]["id"], approval.id);
    let decided=dispatch(&ctx,Profile::Edit,"approval.decide",json!({"approval_id":approval.id,"decision":"deny","reason":"fixture","client":{"id":"fixture","name":"unit test"}})).await?;
    assert_eq!(decided["state"], "denied");
    assert_eq!(
        dispatch(
            &ctx,
            Profile::Read,
            "run.wait",
            json!({"session_id":session.id})
        )
        .await?["status"],
        "completed"
    );
    assert!(!root.path().join("unapproved.txt").exists());
    assert!(
        dispatch(
            &ctx,
            Profile::Read,
            "approval.list",
            json!({"session_id":session.id})
        )
        .await?
        .as_array()
        .unwrap()
        .is_empty()
    );
    Ok(())
}
