//! Small host-side settings operations for the sharing transport.
use super::*;
use crate::remote::{self, CloudflaredInfo, RemotePaths, ShareState};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudflaredSetParams {
    /// None restores managed-copy/PATH discovery. No program is copied.
    #[serde(deserialize_with = "required_path")]
    pub path: Option<PathBuf>,
}

fn required_path<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<PathBuf>, D::Error> {
    Option::<PathBuf>::deserialize(deserializer)
}

#[derive(Clone, Debug, Serialize)]
pub struct SignOutResult {
    pub changed: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct CloudflaredResult {
    pub info: Option<CloudflaredInfo>,
}

fn paths(ctx: &HostCtx) -> Result<RemotePaths, HostError> {
    ctx.store
        .data_dir()
        .map(RemotePaths::in_data_dir)
        .ok_or_else(|| HostError::unavailable("Remote settings need a persistent data directory"))
}

fn selected(ctx: &HostCtx) -> Result<Option<PathBuf>, HostError> {
    let settings = match &ctx.live_config {
        Some(live) => Some(live.accepted().settings()?),
        None => ctx
            .active_providers()
            .map(|active| active.snapshot().settings.clone()),
    };
    Ok(settings.and_then(|settings| settings.remote.cloudflared.map(PathBuf::from)))
}

pub async fn cloudflared_info(ctx: &HostCtx) -> Result<CloudflaredResult, HostError> {
    let paths = paths(ctx)?;
    let chosen = selected(ctx)?;
    let program = match remote::find_cloudflared(chosen.as_deref(), &paths) {
        Ok(program) => program,
        Err(_) if chosen.is_none() => return Ok(CloudflaredResult { info: None }),
        Err(error) => return Err(HostError::invalid(error.to_string())),
    };
    let mut info = remote::cloudflared_info(&program, &paths)
        .await
        .map_err(|error| HostError::invalid(error.to_string()))?;
    info.chosen = chosen.is_some();
    Ok(CloudflaredResult { info: Some(info) })
}

pub async fn cloudflared_set(
    ctx: &HostCtx,
    input: CloudflaredSetParams,
) -> Result<CloudflaredResult, HostError> {
    if ctx.remote_caller {
        return Err(HostError::denied(
            "remote.cloudflared.set (local callers only)",
        ));
    }
    let paths = paths(ctx)?;
    let mut validated = None;
    let value = match input.path {
        Some(path) => {
            let path = path.canonicalize().map_err(|error| {
                HostError::invalid(format!("cloudflared program does not exist: {error}"))
            })?;
            let mut info = remote::cloudflared_info(&path, &paths)
                .await
                .map_err(|error| HostError::invalid(error.to_string()))?;
            info.chosen = true;
            validated = Some(info);
            Some(Value::String(
                path.to_str()
                    .ok_or_else(|| HostError::invalid("cloudflared path must be UTF-8"))?
                    .to_owned(),
            ))
        }
        None => None,
    };
    ctx.providers
        .as_ref()
        .ok_or_else(|| HostError::unavailable("Config updates not available in this host"))?
        .config_set("remote.cloudflared", value.as_ref())
        .await?;
    match validated {
        Some(info) => Ok(CloudflaredResult { info: Some(info) }),
        None => cloudflared_info(ctx).await,
    }
}

pub fn sign_out(_ctx: &HostCtx) -> Result<SignOutResult, HostError> {
    Ok(SignOutResult {
        changed: remote::forget_cloudflare_login(None)?,
    })
}

pub async fn share_state(ctx: &HostCtx) -> Result<ShareState, HostError> {
    let handle = ctx.remote_share.lock().unwrap().clone();
    if let Some(handle) = handle {
        let state = handle.state();
        if state.status.is_some() {
            return Ok(state);
        }
    }
    Ok(remote::running_share_state(&paths(ctx)?)
        .await?
        .unwrap_or_default())
}

pub(super) async fn command(
    ctx: &HostCtx,
    method: MethodId,
    params: Value,
) -> Result<Value, HostError> {
    // Locality precedes even parameter decoding and filesystem access.
    if method == MethodId::RemoteCloudflaredSet && ctx.remote_caller {
        return Err(HostError::denied(
            "remote.cloudflared.set (local callers only)",
        ));
    }
    match method {
        MethodId::RemoteCloudflareSignOut => {
            let _: EmptyParams = decode(params)?;
            encode(sign_out(ctx)?)
        }
        MethodId::RemoteCloudflaredInfo => {
            let _: EmptyParams = decode(params)?;
            encode(cloudflared_info(ctx).await?)
        }
        MethodId::RemoteCloudflaredSet => encode(cloudflared_set(ctx, decode(params)?).await?),
        MethodId::RemoteShareState => {
            let _: EmptyParams = decode(params)?;
            encode(share_state(ctx).await?)
        }
        _ => unreachable!("remote settings method table"),
    }
}

#[cfg(test)]
mod input_tests {
    use super::*;

    #[test]
    fn selection_requires_an_explicit_path_or_null() {
        for input in [
            json!({}),
            json!({"path":null,"extra":true}),
            json!({"path":true}),
        ] {
            assert!(serde_json::from_value::<CloudflaredSetParams>(input).is_err());
        }
        assert!(
            serde_json::from_value::<CloudflaredSetParams>(json!({"path":null}))
                .unwrap()
                .path
                .is_none()
        );
        assert_eq!(
            serde_json::from_value::<CloudflaredSetParams>(json!({"path":"chosen"}))
                .unwrap()
                .path,
            Some(PathBuf::from("chosen"))
        );
    }
}
