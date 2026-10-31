//! MCP server list with live and sign-in state, and the OAuth sign-in
//! controls. Server settings themselves go through `config.set` on
//! `mcp.servers.<name>.*`; the host opens no browser.
use super::*;
use bubblepaw_mcp::{McpHost, McpServerStatus};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ServerParams {
    pub server: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AuthCompleteParams {
    pub server: String,
    pub redirect_url: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct ServersListResult {
    pub servers: Vec<McpServerStatus>,
}
#[derive(Clone, Debug, Serialize)]
pub struct AuthStartResult {
    pub url: String,
}

fn host(ctx: &HostCtx) -> Result<&McpHost, HostError> {
    Ok(required(&ctx.live_config, "MCP servers")?.mcp())
}

fn status(host: &McpHost, server: &str) -> Result<Value, HostError> {
    let status = host.servers().into_iter().find(|s| s.name == server);
    encode(
        status
            .ok_or_else(|| HostError::invalid(format!("MCP server {server} is not configured")))?,
    )
}

/// Publish the tools a sign-in change added or removed.
async fn publish(ctx: &HostCtx) {
    if let Some(live) = &ctx.live_config {
        live.check().await;
    }
}

pub(super) async fn command(
    ctx: &HostCtx,
    method: MethodId,
    params: Value,
) -> Result<Value, HostError> {
    let fail = |error: anyhow::Error| HostError::invalid(error.to_string());
    match method {
        MethodId::McpServersList => encode(ServersListResult {
            servers: host(ctx)?.servers(),
        }),
        MethodId::McpAuthStart => {
            let input: ServerParams = decode(params)?;
            let url = host(ctx)?.auth_start(&input.server).await.map_err(fail)?;
            encode(AuthStartResult { url })
        }
        MethodId::McpAuthComplete => {
            let input: AuthCompleteParams = decode(params)?;
            let mcp = host(ctx)?;
            mcp.auth_complete(&input.server, &input.redirect_url)
                .await
                .map_err(fail)?;
            publish(ctx).await;
            status(mcp, &input.server)
        }
        MethodId::McpAuthSignOut => {
            let input: ServerParams = decode(params)?;
            let mcp = host(ctx)?;
            mcp.auth_sign_out(&input.server).await.map_err(fail)?;
            publish(ctx).await;
            status(mcp, &input.server)
        }
        _ => unreachable!("mcp method table"),
    }
}
