use super::*;
use anyhow::Result;
use bubblepaw_core::{
    ApprovalMode, ApprovalPolicy, ApprovalState, EventKind, HarnessConfig, ToolCall, ToolRegistry,
};
use std::collections::BTreeMap;

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

/// Runs one `approval_write` call under a host policy and run options. Returns the
/// state of the first approval record, the run's final status and whether the tool
/// wrote its file. A `Pending` approval is denied so the run can finish.
async fn approval_write_under(
    host: ApprovalPolicy,
    run: ApprovalPolicy,
) -> Result<(Option<ApprovalState>, String, bool)> {
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
        "policy handled",
    );
    let config = HarnessConfig {
        approval: host,
        ..Default::default()
    };
    let harness = Arc::new(Harness::new(
        store.clone(),
        Arc::new(provider),
        registry,
        config,
    ));
    let ctx = HostCtx::new(harness, store, root.path());
    let mut events = events::subscribe(&ctx, None, Some(&session.id))?.receiver;
    dispatch(
        &ctx,
        Profile::Edit,
        "run.start",
        json!({"session_id":session.id,"text":"request a write","options":{"approval":run}}),
    )
    .await?;
    let mut initial = None;
    let status = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match events.recv().await?.kind {
                EventKind::ApprovalRequested { approval } => {
                    initial = Some(approval.state);
                    if approval.state == ApprovalState::Pending {
                        dispatch(
                            &ctx,
                            Profile::Edit,
                            "approval.decide",
                            json!({"approval_id":approval.id,"decision":"deny","reason":"fixture","client":{"id":"fixture","name":"unit test"}}),
                        )
                        .await?;
                    }
                }
                EventKind::RunFinished { status, .. } => return Ok::<_, anyhow::Error>(status),
                _ => {}
            }
        }
    })
    .await??;
    Ok((initial, status, root.path().join("unapproved.txt").exists()))
}

#[tokio::test]
async fn run_start_options_only_tighten_host_approval_policy() -> Result<()> {
    let run_allows = ApprovalPolicy {
        confirm: ApprovalMode::Allow,
        tools: BTreeMap::from([("approval_write".to_owned(), ApprovalMode::Allow)]),
        ..Default::default()
    };
    // (case, host policy, run options, state of the first approval record)
    let cases = [
        (
            "inherited per-tool deny",
            ApprovalPolicy {
                tools: BTreeMap::from([("approval_write".to_owned(), ApprovalMode::Deny)]),
                ..Default::default()
            },
            run_allows.clone(),
            ApprovalState::Denied,
        ),
        (
            "inherited command confirm",
            ApprovalPolicy {
                confirm: ApprovalMode::Ask,
                ..Default::default()
            },
            run_allows,
            ApprovalState::Pending,
        ),
        (
            "run-added deny",
            ApprovalPolicy::default(),
            ApprovalPolicy {
                deny: vec!["approval_write".into()],
                ..Default::default()
            },
            ApprovalState::Denied,
        ),
    ];
    // Every case runs before any assertion, so one report lists all regressions.
    let mut failures = Vec::new();
    for (name, host, run, expected) in cases {
        let (initial, status, wrote) = approval_write_under(host, run).await?;
        if (initial, status.as_str(), wrote) != (Some(expected), "completed", false) {
            failures.push(format!(
                "{name}: expected {expected:?} and no write, got {initial:?}, run {status}, wrote={wrote}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    Ok(())
}
