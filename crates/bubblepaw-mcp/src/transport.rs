//! Small host boundary between child stdio and Streamable HTTP.
use crate::{McpClientEnv, McpServerSettings, http::HttpConnection, protocol};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};

const STDIO_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

pub(crate) enum Connection {
    Stdio(Arc<protocol::Connection>),
    Http(Arc<HttpConnection>),
}

impl Connection {
    pub(crate) fn open(
        config: &McpServerSettings,
        workspace: &Path,
        allowed_envs: &[String],
        client_env: Option<&McpClientEnv>,
    ) -> Result<Arc<Self>> {
        config.validate(allowed_envs)?;
        let connection = if config.url.is_some() {
            Self::Http(HttpConnection::new(config, allowed_envs, client_env)?)
        } else {
            Self::Stdio(protocol::Connection::spawn(config, workspace, client_env)?)
        };
        Ok(Arc::new(connection))
    }

    pub(crate) async fn initialize(&self) -> Result<()> {
        match self {
            Self::Http(connection) => connection.initialize().await,
            Self::Stdio(connection) => {
                let result = connection
                    .request(
                        "initialize",
                        json!({
                            "protocolVersion": STDIO_VERSIONS[0],
                            "capabilities": {},
                            "clientInfo": {"name":"bubblepaw", "version":env!("CARGO_PKG_VERSION")},
                        }),
                    )
                    .await?;
                let version = result["protocolVersion"].as_str().unwrap_or("");
                ensure!(
                    STDIO_VERSIONS.contains(&version),
                    "incompatible MCP protocol version"
                );
                ensure!(
                    result["capabilities"]["tools"].is_object(),
                    "MCP server does not support tools"
                );
                connection.notify("notifications/initialized", json!({}))
            }
        }
    }

    pub(crate) fn register_tool(&self, name: &str, schema: &Value) -> Result<()> {
        match self {
            Self::Stdio(_) => Ok(()),
            Self::Http(connection) => connection.register_tool(name, schema),
        }
    }

    pub(crate) async fn request(self: &Arc<Self>, method: &str, params: Value) -> Result<Value> {
        match self.as_ref() {
            Self::Stdio(connection) => connection.request(method, params).await,
            Self::Http(connection) => connection.request(method, params).await,
        }
    }

    pub(crate) async fn call_tool(
        self: &Arc<Self>,
        name: &str,
        input: Value,
        schema: &Value,
    ) -> Result<Value> {
        match self.as_ref() {
            Self::Stdio(connection) => {
                connection
                    .request("tools/call", json!({"name":name,"arguments":input}))
                    .await
            }
            Self::Http(connection) => connection.call_tool(name, input, schema).await,
        }
    }

    pub(crate) fn take_tools_changed(&self) -> bool {
        match self {
            Self::Stdio(c) => c.take_tools_changed(),
            Self::Http(c) => c.take_tools_changed(),
        }
    }
    pub(crate) fn mark_tools_changed(&self) {
        match self {
            Self::Stdio(c) => c.mark_tools_changed(),
            Self::Http(c) => c.mark_tools_changed(),
        }
    }
    pub(crate) fn alive(&self) -> bool {
        match self {
            Self::Stdio(connection) => connection.alive(),
            Self::Http(connection) => connection.alive(),
        }
    }

    pub(crate) fn stderr(&self) -> String {
        match self {
            Self::Stdio(connection) => connection.stderr(),
            Self::Http(_) => String::new(),
        }
    }

    pub(crate) fn redact(&self, text: &str) -> String {
        match self {
            Self::Stdio(connection) => connection.redact(text),
            Self::Http(connection) => connection.redact(text),
        }
    }

    pub(crate) fn stop(&self, grace: Duration) {
        match self {
            Self::Stdio(connection) => connection.stop(grace),
            Self::Http(connection) => connection.stop(grace),
        }
    }

    pub(crate) async fn shutdown(&self, grace: Duration) {
        match self {
            Self::Stdio(connection) => connection.shutdown(grace).await,
            Self::Http(connection) => connection.shutdown(grace).await,
        }
    }
}
