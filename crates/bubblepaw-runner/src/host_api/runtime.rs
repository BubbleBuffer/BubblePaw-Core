use super::*;
use crate::session_control::SlashCommand;

pub(super) async fn slash(
    ctx: &HostCtx,
    profile: Profile,
    id: Option<&str>,
    command: SlashCommand<'_>,
) -> Result<Value, HostError> {
    let method = match command {
        SlashCommand::Reload(_) => MethodId::ConfigReload,
        SlashCommand::Rename(_) => MethodId::SessionRename,
        SlashCommand::Rewind(_) => MethodId::SessionRewind,
        SlashCommand::Compact(_) => MethodId::SessionCompact,
    };
    if profile < method.profile() {
        return Err(HostError::denied(method.name()));
    }
    if method == MethodId::SessionCompact {
        ctx.require_model()?;
    }
    ctx.control().execute(id, command).await.map_err(Into::into)
}
pub(super) async fn command(
    ctx: &HostCtx,
    profile: Profile,
    method: MethodId,
    params: Value,
) -> Result<Value, HostError> {
    match method {
        MethodId::RunStart
        | MethodId::RunResume
        | MethodId::RunSteer
        | MethodId::RunFollowUp
        | MethodId::RunCancel
        | MethodId::RunWait => super::runs::command(ctx, profile, method, params).await,
        MethodId::QuestionAnswer | MethodId::QuestionAnswerChild => {
            super::questions::command(ctx, method, params).await
        }
        MethodId::HostCommand => {
            let input: HostCommandParams = decode(params)?;
            let Some(command) = SlashCommand::parse(&input.text) else {
                return Ok(Value::Null);
            };
            if command.needs_session() && input.session_id.is_none() {
                return Ok(json!({"needs_session":true}));
            }
            slash(ctx, profile, input.session_id.as_deref(), command).await
        }
        MethodId::ChildCancel => {
            let input: ChildParams = decode(params)?;
            ctx.ensure_session(&input.parent)?;
            ctx.ensure_session(&input.child)?;
            required(&ctx.agents, "Child controls")?
                .cancel(&input.parent, &input.child)
                .await
                .map_err(Into::into)
        }
        MethodId::SessionDelete => {
            let input: SessionParams = decode(params)?;
            ctx.ensure_session(&input.session_id)?;
            if let Some(wake) = ctx.wake_coordinator() {
                wake.suppress(&input.session_id);
            }
            let (store, id) = (ctx.store.clone(), input.session_id.clone());
            let worktrees =
                tokio::task::spawn_blocking(move || crate::worktrees::delete_session(&store, &id))
                    .await
                    .map_err(|e| HostError::internal(e.to_string()))??;
            if let Some(processes) = &ctx.processes {
                processes.cancel_owner(&input.session_id).await;
            }
            encode(DeleteResult {
                deleted: input.session_id,
                worktrees,
            })
        }
        MethodId::SessionRename => {
            let input: RenameParams = decode_control(params, "title is required")?;
            slash(
                ctx,
                profile,
                Some(&input.session_id),
                SlashCommand::Rename(&input.title),
            )
            .await
        }
        MethodId::SessionRewind => {
            let input: RewindParams = decode_control(params, "message_id is required")?;
            slash(
                ctx,
                profile,
                Some(&input.session_id),
                SlashCommand::Rewind(&input.message_id),
            )
            .await
        }
        MethodId::SessionCompact => {
            let input: CompactParams = decode_control(params, "focus must be a string")?;
            slash(
                ctx,
                profile,
                Some(&input.session_id),
                SlashCommand::Compact(input.focus.as_deref()),
            )
            .await
        }
        _ => unreachable!("runtime method table"),
    }
}

fn decode_control<T: serde::de::DeserializeOwned>(
    params: Value,
    field_error: &str,
) -> Result<T, HostError> {
    let error = match decode(params.clone()) {
        Ok(input) => return Ok(input),
        Err(error) => error,
    };
    // Keep protocol 0.1's operation-specific text once the session field is valid.
    if decode::<SessionParams>(params).is_ok() {
        Err(HostError::invalid(field_error))
    } else {
        Err(error)
    }
}
