use super::*;
#[derive(Deserialize, Serialize)]
pub struct DecideParams {
    pub approval_id: String,
    pub decision: String,
    pub reason: Option<String>,
    pub client: Option<Value>,
}
pub type ListResult = Vec<bubblepaw_core::ApprovalRecord>;
pub type DecideResult = bubblepaw_core::ApprovalRecord;
pub fn list(ctx: &HostCtx, input: SessionParams) -> Result<ListResult, HostError> {
    ctx.ensure_read_session(&input.session_id)?;
    ctx.harness
        .pending_approvals(&input.session_id)
        .map_err(Into::into)
}
pub fn decide(ctx: &HostCtx, input: DecideParams) -> Result<DecideResult, HostError> {
    let decision = bubblepaw_core::Decision::parse(&input.decision, input.reason)?;
    let client = bubblepaw_core::client_param(input.client.as_ref())
        .map_err(|e| HostError::invalid(e.to_string()))?;
    ctx.harness
        .decide_from(&input.approval_id, decision, client)
        .map_err(Into::into)
}

/// A one-shot host has no interactive client. Keep its approval policy here,
/// including hidden children when recovering from a lagged event receiver.
pub fn deny_unattended(
    ctx: HostCtx,
    reason: String,
) -> Result<tokio::task::JoinHandle<()>, HostError> {
    let mut receiver = super::events::subscribe(&ctx, None, None)?.receiver;
    Ok(tokio::spawn(async move {
        let deny = |id: String| {
            let _ = decide(
                &ctx,
                DecideParams {
                    approval_id: id,
                    decision: "deny".into(),
                    reason: Some(reason.clone()),
                    client: None,
                },
            );
        };
        loop {
            match receiver.recv().await {
                Ok(bubblepaw_core::HarnessEvent {
                    kind: bubblepaw_core::EventKind::ApprovalRequested { approval },
                    ..
                }) if approval.state == bubblepaw_core::ApprovalState::Pending
                    && ctx.scope.contains(&ctx.store, &approval.session_id) =>
                {
                    deny(approval.id)
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    for session in ctx
                        .store
                        .list_sessions()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|session| ctx.scope.contains(&ctx.store, &session.id))
                    {
                        for approval in ctx
                            .harness
                            .pending_approvals(&session.id)
                            .unwrap_or_default()
                        {
                            deny(approval.id);
                        }
                    }
                }
                Err(_) => break,
            }
        }
    }))
}
pub(super) fn command(ctx: &HostCtx, method: MethodId, params: Value) -> Result<Value, HostError> {
    if method == MethodId::ApprovalList {
        encode(list(ctx, decode(params)?)?)
    } else {
        encode(decide(ctx, decode(params)?)?)
    }
}
