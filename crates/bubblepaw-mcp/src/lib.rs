//! Host-owned stdio and Streamable HTTP MCP client. Every tool is a durable, approval-gated
//! effect; retries are only for establishing the next call's connection.
mod client_env;
mod config;
mod host;
mod http;
mod oauth;
mod protocol;
mod transport;

pub use client_env::McpClientEnv;
pub use config::{FieldError, McpServerSettings, McpSettings, McpToolSettings};
pub use host::{
    McpAuthState, McpAuthStatus, McpDiff, McpHost, McpHostOptions, McpServerState, McpServerStatus,
    McpToolStatus, tool_name,
};
