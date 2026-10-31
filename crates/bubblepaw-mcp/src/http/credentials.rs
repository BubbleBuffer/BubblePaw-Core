//! Request headers from `headers_from` (resolved from the environment) and from
//! the stored header secrets, with every secret value collected for redaction.
use crate::{McpClientEnv, McpServerSettings};
use anyhow::{Result, anyhow, ensure};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::collections::BTreeMap;

pub(super) fn build(
    config: &McpServerSettings,
    client_env: Option<&McpClientEnv>,
    stored: &BTreeMap<String, String>,
) -> Result<(HeaderMap, Vec<String>)> {
    let mut headers = HeaderMap::new();
    let mut secrets = Vec::new();
    for (name, variable) in &config.headers_from {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| anyhow!("invalid MCP HTTP credential header name"))?;
        // A supplied client snapshot is the only source: no host fallback.
        // The missing-variable text is the standalone text, on purpose.
        let missing = || anyhow!("required MCP HTTP credential environment variable is missing");
        let secret = match client_env {
            Some(client_env) => client_env
                .get(variable)
                .map(str::to_owned)
                .ok_or_else(missing)?,
            None => std::env::var(variable).map_err(|_| missing())?,
        };
        ensure!(
            !secret.is_empty(),
            "MCP HTTP credential environment variable is empty"
        );
        let mut value = HeaderValue::from_str(&secret)
            .map_err(|_| anyhow!("invalid MCP HTTP credential header value"))?;
        value.set_sensitive(true);
        // Bearer credentials are commonly echoed without the scheme.
        if name == reqwest::header::AUTHORIZATION
            && let Some((_, token)) = secret.split_once(' ')
            && !token.is_empty()
        {
            secrets.push(token.to_owned());
        }
        secrets.push(secret);
        headers
            .try_insert(name, value)
            .map_err(|_| anyhow!("MCP HTTP credential header count exceeds the limit"))?;
    }
    // Header values set with `mcp.headers.set` win over `headers_from`.
    for (name, secret) in stored {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| anyhow!("invalid MCP HTTP stored header name"))?;
        ensure!(!secret.is_empty(), "MCP HTTP stored header value is empty");
        let mut value = HeaderValue::from_str(secret)
            .map_err(|_| anyhow!("invalid MCP HTTP stored header value"))?;
        value.set_sensitive(true);
        if name == reqwest::header::AUTHORIZATION
            && let Some((_, token)) = secret.split_once(' ')
            && !token.is_empty()
        {
            secrets.push(token.to_owned());
        }
        secrets.push(secret.clone());
        headers
            .try_insert(name, value)
            .map_err(|_| anyhow!("MCP HTTP credential header count exceeds the limit"))?;
    }
    headers
        .try_reserve(8)
        .map_err(|_| anyhow!("MCP HTTP credential header count exceeds the limit"))?;
    Ok((headers, secrets))
}
