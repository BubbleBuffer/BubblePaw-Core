//! Shape checks for `[mcp]` before typed deserialization. A refusal the checks
//! can tie to one leaf of one server is a [`LeafError`], so `config.set` can
//! name that leaf.
use super::valid_id;
use anyhow::{Context, Result, ensure};
use bubblepaw_mcp::{FieldError, McpSettings};
use std::fmt;
use toml::Value;

/// A config refusal about one leaf: `key` is its full dotted key, such as
/// `mcp.servers.docs.url`. The message is the same text a plain error carries.
#[derive(Debug)]
pub struct LeafError {
    pub key: String,
    message: String,
}
impl LeafError {
    pub(super) fn new(key: String, message: String) -> Self {
        Self { key, message }
    }
}
impl fmt::Display for LeafError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for LeafError {}

/// `ensure!` for a leaf: the error carries the leaf's key.
macro_rules! ensure_leaf {
    ($cond:expr, $key:expr, $($message:tt)+) => {
        if !$cond {
            return Err(LeafError::new($key, format!($($message)+)).into());
        }
    };
}

/// Secrets are never read here: `env_from` entries are checked as environment
/// names only.
pub(super) fn validate_mcp(mcp: Option<&Value>) -> Result<()> {
    let settings: McpSettings = mcp
        .cloned()
        .unwrap_or_else(|| Value::Table(toml::map::Map::new()))
        .try_into()
        .map_err(|_| anyhow::anyhow!("Invalid config value: mcp; invalid field type"))?;
    // Validate the allowlist even when no servers are configured.
    let probe = bubblepaw_mcp::McpServerSettings {
        command: "validation-only".into(),
        ..Default::default()
    };
    probe.validate(&settings.header_key_envs).map_err(|_| {
        anyhow::anyhow!(
            "Invalid config value: mcp.header_key_envs; expected environment variable identifiers"
        )
    })?;
    let Some(servers) = mcp.and_then(|mcp| mcp.get("servers")) else {
        return Ok(());
    };
    let servers = servers.as_table().context("mcp.servers must be a table")?;
    for (name, server) in servers {
        ensure!(valid_id(name), "Invalid config value: mcp.servers.{name}");
        let server = server
            .as_table()
            .with_context(|| format!("Invalid config value: mcp.servers.{name}"))?;
        settings.servers[name]
            .validate(&settings.header_key_envs)
            .map_err(|error| entry_error(error, name))?;
        for key in ["startup_timeout_secs", "call_timeout_secs"] {
            if let Some(seconds) = server.get(key) {
                ensure_leaf!(
                    seconds.as_integer().is_some_and(|seconds| seconds > 0),
                    format!("mcp.servers.{name}.{key}"),
                    "Invalid config value: mcp.servers.{name}.{key}; must be positive"
                );
            }
        }
        validate_mcp_approval(
            server.get("approval"),
            &format!("mcp.servers.{name}.approval"),
        )?;
        if let Some(env) = server.get("env").and_then(Value::as_table) {
            for key in env.keys() {
                ensure_leaf!(
                    env_name(key),
                    format!("mcp.servers.{name}.env"),
                    "Invalid config value: mcp.servers.{name}.env"
                );
            }
        }
        if let Some(env_from) = server.get("env_from").and_then(Value::as_table) {
            // The value is the host variable's name; its secret is read at spawn time.
            for (key, value) in env_from {
                ensure_leaf!(
                    env_name(key) && value.as_str().is_some_and(env_name),
                    format!("mcp.servers.{name}.env_from"),
                    "Invalid config value: mcp.servers.{name}.env_from; expected environment variable names"
                );
            }
        }
        if let Some(tools) = server.get("tools") {
            let tools = tools
                .as_table()
                .with_context(|| format!("Invalid config value: mcp.servers.{name}.tools"))?;
            for (tool, settings) in tools {
                if let Some(settings) = settings.as_table() {
                    validate_mcp_approval(
                        settings.get("approval"),
                        &format!("mcp.servers.{name}.tools.{tool}.approval"),
                    )?;
                }
            }
        }
    }
    Ok(())
}

/// A server's own refusal, worded as before; a field-level one also names its leaf.
fn entry_error(error: anyhow::Error, name: &str) -> anyhow::Error {
    let message = format!("Invalid config value: mcp.servers.{name}: {error:#}");
    match error.downcast_ref::<FieldError>() {
        Some(field) => {
            LeafError::new(format!("mcp.servers.{name}.{}", field.field), message).into()
        }
        None => anyhow::anyhow!(message),
    }
}

fn validate_mcp_approval(value: Option<&Value>, path: &str) -> Result<()> {
    if let Some(value) = value {
        ensure_leaf!(
            matches!(value.as_str(), Some("confirm" | "none")),
            path.to_owned(),
            "Invalid config value: {path}; expected confirm or none"
        );
    }
    Ok(())
}

fn env_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['=', '\0'])
}
