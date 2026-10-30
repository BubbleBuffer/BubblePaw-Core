use super::*;
use crate::session_control::SlashCommand;
use bubblepaw_core::MessageOrigin;
pub type StartParams = RunParams;
#[derive(Deserialize, Serialize)]
pub struct ResumeParams {
    pub session_id: String,
    #[serde(default)]
    pub options: bubblepaw_core::RunOptions,
    #[serde(default)]
    pub client: Option<Value>,
}
pub type SteerParams = TextParams;
pub type FollowUpParams = TextParams;
pub type StartResult = bubblepaw_core::Run;
pub type ResumeResult = bubblepaw_core::Run;
pub type WaitResult = bubblepaw_core::Run;
pub type SteerResult = bubblepaw_core::QueuedInput;
pub type FollowUpResult = bubblepaw_core::QueuedInput;
pub type CancelResult = RequestedResult;

async fn prepare(
    ctx: &HostCtx,
    id: &str,
    text: String,
    attachments: Vec<String>,
) -> Result<String, HostError> {
    if let Some(files) = ctx.input_files.clone() {
        let id = id.to_owned();
        return tokio::task::spawn_blocking(move || files.prepare(&id, &text, &attachments))
            .await
            .map_err(|e| HostError::internal(e.to_string()))?
            .map_err(|e| HostError::invalid(e.to_string()));
    }
    if !attachments.is_empty() {
        return Err(HostError::unavailable("Input attachments are unavailable"));
    }
    ctx.input_limits
        .validate_prompt(&text)
        .map_err(|e| HostError::invalid(e.to_string()))?;
    Ok(text)
}
fn wake(ctx: &HostCtx, id: &str) {
    if let Some(wake) = ctx.wake_coordinator() {
        wake.user_input(id);
    }
}
pub async fn wait(ctx: &HostCtx, input: SessionParams) -> Result<WaitResult, HostError> {
    ctx.ensure_read_session(&input.session_id)?;
    ctx.harness
        .wait_for_idle(&input.session_id)
        .await
        .map_err(Into::into)
}
pub async fn cancel(ctx: &HostCtx, input: CancelParams) -> Result<CancelResult, HostError> {
    let client = bubblepaw_core::client_param(input.client.as_ref())
        .map_err(|e| HostError::invalid(e.to_string()))?;
    Ok(RequestedResult {
        requested: ctx.control().cancel(&input.session_id, client).await?,
    })
}
pub async fn resume(ctx: &HostCtx, input: ResumeParams) -> Result<ResumeResult, HostError> {
    ctx.ensure_session(&input.session_id)?;
    ctx.require_model()?;
    let mut options = input.options;
    options.client = bubblepaw_core::client_param(input.client.as_ref())
        .map_err(|e| HostError::invalid(e.to_string()))?;
    let (ctx, id) = (ctx.clone(), input.session_id);
    tokio::task::spawn_blocking(move || {
        ctx.scope.admit(&ctx.store, &id, ctx.hold_admission, || {
            ctx.harness.start_resume(&id, options)
        })
    })
    .await
    .map_err(|e| HostError::internal(e.to_string()))?
    .map_err(Into::into)
}
/// Admit ordinary prepared input; the method table intercepts slash commands
/// before calling this typed operation.
pub async fn start(ctx: &HostCtx, input: StartParams) -> Result<StartResult, HostError> {
    ctx.ensure_session(&input.session_id)?;
    ctx.require_model()?;
    let mut options = input.options;
    options.client = bubblepaw_core::client_param(input.client.as_ref())
        .map_err(|e| HostError::invalid(e.to_string()))?;
    options.origin = MessageOrigin::Typed;
    if let Some(script) = input.script {
        options.script = Some(script);
    }
    let text = prepare(
        ctx,
        &input.session_id,
        input
            .text
            .ok_or_else(|| HostError::invalid("text is required"))?,
        input.attachments,
    )
    .await?;
    let (admitting, id) = (ctx.clone(), input.session_id.clone());
    let run = tokio::task::spawn_blocking(move || {
        admitting
            .scope
            .admit(&admitting.store, &id, admitting.hold_admission, || {
                admitting.harness.start(&id, text, options)
            })
    })
    .await
    .map_err(|e| HostError::internal(e.to_string()))??;
    wake(ctx, &input.session_id);
    Ok(run)
}
async fn queue(ctx: &HostCtx, input: TextParams, steering: bool) -> Result<SteerResult, HostError> {
    ctx.ensure_session(&input.session_id)?;
    let client = bubblepaw_core::client_param(input.client.as_ref())
        .map_err(|e| HostError::invalid(e.to_string()))?;
    let text = prepare(ctx, &input.session_id, input.text, input.attachments).await?;
    let result = if steering {
        ctx.harness.steer_from(&input.session_id, text, client)?
    } else {
        ctx.harness.followup_from(&input.session_id, text, client)?
    };
    wake(ctx, &input.session_id);
    Ok(result)
}
pub async fn steer(ctx: &HostCtx, input: SteerParams) -> Result<SteerResult, HostError> {
    queue(ctx, input, true).await
}
pub async fn follow_up(ctx: &HostCtx, input: FollowUpParams) -> Result<FollowUpResult, HostError> {
    queue(ctx, input, false).await
}
pub(super) async fn command(
    ctx: &HostCtx,
    profile: Profile,
    method: MethodId,
    params: Value,
) -> Result<Value, HostError> {
    match method {
        MethodId::RunWait => return encode(wait(ctx, decode(params)?).await?),
        MethodId::RunCancel => return encode(cancel(ctx, decode(params)?).await?),
        MethodId::RunResume => return encode(resume(ctx, decode(params)?).await?),
        _ => {}
    }
    let input: RunParams = decode(params)?;
    let text = input
        .text
        .as_deref()
        .ok_or_else(|| HostError::invalid("text is required"))?;
    // Protocol 0.1 rejects oversized input before looking up even a missing
    // session. Reuse the attachment-aware limit without touching host state.
    ctx.input_files
        .as_ref()
        .map_or(ctx.input_limits, |files| files.limits())
        .validate_message(text, !input.attachments.is_empty())
        .map_err(|error| HostError::invalid(error.to_string()))?;
    ctx.ensure_session(&input.session_id)?;
    if input.attachments.is_empty()
        && let Some(command) = SlashCommand::parse(text)
    {
        return super::runtime::slash(ctx, profile, Some(&input.session_id), command).await;
    }
    if method == MethodId::RunStart {
        return encode(start(ctx, input).await?);
    }
    let input = TextParams {
        session_id: input.session_id,
        text: input.text.expect("validated text"),
        attachments: input.attachments,
        client: input.client,
    };
    if method == MethodId::RunSteer {
        encode(steer(ctx, input).await?)
    } else {
        encode(follow_up(ctx, input).await?)
    }
}
