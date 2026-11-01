use anyhow::{Result, ensure};
use bubblepaw_core::{
    Approval,
    config_schema::{LIVE, Node},
    config_section,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt, path::PathBuf};

/// A refusal that concerns one field of a server entry (`url`, `command`,
/// `headers_from`...). The message is the text a plain error would carry; the
/// field lets a caller point at the right input.
#[derive(Debug)]
pub struct FieldError {
    pub field: &'static str,
    message: &'static str,
}
impl FieldError {
    fn new(field: &'static str, message: &'static str) -> Self {
        Self { field, message }
    }
}
impl fmt::Display for FieldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message)
    }
}
impl std::error::Error for FieldError {}

macro_rules! ensure_field {
    ($cond:expr, $field:expr, $message:literal) => {
        if !$cond {
            return Err(FieldError::new($field, $message).into());
        }
    };
}

config_section! {
    #[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
    #[serde(default)]
    pub struct McpSettings {
        pub header_key_envs: Vec<String> => LIVE,
        pub servers: BTreeMap<String, McpServerSettings> =>
            Node::Map(&Node::Table(McpServerSettings::FIELDS)),
    }
}
impl McpSettings {
    pub fn validate(&self) -> Result<()> {
        let mut enabled = self.servers.values().filter(|c| c.enabled);
        enabled.try_for_each(|c| c.validate(&self.header_key_envs))
    }
}

config_section! {
    /// Secret values are resolved only at connection time, never during config rendering.
    #[derive(Clone, Serialize, Deserialize, PartialEq)]
    #[serde(default)]
    pub struct McpServerSettings {
        pub command: String => LIVE,
        pub url: Option<String> => LIVE,
        /// Header names may contain dots, so they are free-form keys.
        pub headers_from: BTreeMap<String, String> => Node::Free(true),
        /// Sign in with the MCP authorization flow (OAuth 2.1). HTTP only.
        pub oauth: bool => LIVE,
        pub args: Vec<String> => LIVE,
        pub cwd: Option<PathBuf> => LIVE,
        pub env: BTreeMap<String, String> => Node::Map(&LIVE),
        pub env_from: BTreeMap<String, String> => Node::Map(&LIVE),
        pub enabled: bool => LIVE,
        /// Discover on demand instead of declaring every server tool to the model.
        /// Set false for servers whose tools should always be native declarations.
        pub defer: bool => LIVE,
        pub approval: Approval => LIVE,
        pub startup_timeout_secs: u64 => LIVE,
        pub call_timeout_secs: u64 => LIVE,
        /// Optional allowlist. Omitted permits all; Some(empty) permits none.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub include_tools: Option<Vec<String>> => LIVE,
        pub exclude_tools: Vec<String> => LIVE,
        pub tools: BTreeMap<String, McpToolSettings> =>
            Node::Map(&Node::Table(McpToolSettings::FIELDS)),
    }
}

impl Default for McpServerSettings {
    fn default() -> Self {
        Self {
            command: String::new(),
            url: None,
            headers_from: BTreeMap::new(),
            oauth: false,
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            env_from: BTreeMap::new(),
            enabled: true,
            defer: true,
            approval: Approval::Confirm,
            startup_timeout_secs: 30,
            call_timeout_secs: 120,
            include_tools: None,
            exclude_tools: Vec::new(),
            tools: BTreeMap::new(),
        }
    }
}

impl fmt::Debug for McpServerSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpServerSettings")
            .field(
                "transport",
                &if self.url.is_some() { "http" } else { "stdio" },
            )
            .field("cwd", &self.cwd)
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .field(
                "env_from_names",
                &self
                    .env_from
                    .values()
                    .filter(|name| valid_header_env(name))
                    .collect::<Vec<_>>(),
            )
            .field(
                "headers_from_keys",
                &self.headers_from.keys().collect::<Vec<_>>(),
            )
            .field("oauth", &self.oauth)
            .field("enabled", &self.enabled)
            .field("defer", &self.defer)
            .field("approval", &self.approval)
            .field("startup_timeout_secs", &self.startup_timeout_secs)
            .field("call_timeout_secs", &self.call_timeout_secs)
            .field("include_tools", &self.include_tools)
            .field("exclude_tools", &self.exclude_tools)
            .field("tools", &self.tools)
            .finish()
    }
}

impl McpServerSettings {
    pub(crate) fn allows_tool(&self, name: &str) -> bool {
        let included = self.include_tools.as_ref().is_none_or(|patterns| {
            patterns
                .iter()
                .any(|pattern| tool_pattern_matches(pattern, name))
        });
        included
            && !self
                .exclude_tools
                .iter()
                .any(|pattern| tool_pattern_matches(pattern, name))
    }

    /// Inventory filters and deferral are host settings and need no new transport.
    pub(crate) fn same_connection_settings(&self, other: &Self) -> bool {
        let mut current = self.clone();
        current.defer = other.defer;
        current.include_tools = other.include_tools.clone();
        current.exclude_tools = other.exclude_tools.clone();
        current == *other
    }

    /// Validate before resolving credentials or opening either transport.
    /// Diagnostics name fields only, including for malformed secret references.
    /// A refusal that concerns one field of the entry is a [`FieldError`].
    pub fn validate(&self, header_key_envs: &[String]) -> Result<()> {
        ensure!(
            header_key_envs.iter().all(|name| valid_header_env(name)),
            "header_key_envs must contain environment variable identifiers"
        );
        let command = !self.command.trim().is_empty();
        // Both set blames `url`, neither blames `command`: the field the owner
        // most likely meant for the server's type.
        ensure_field!(
            command != self.url.is_some(),
            if command { "url" } else { "command" },
            "exactly one of command or url is required"
        );
        ensure_field!(
            self.startup_timeout_secs > 0,
            "startup_timeout_secs",
            "startup_timeout_secs must be positive"
        );
        ensure_field!(
            self.call_timeout_secs > 0,
            "call_timeout_secs",
            "call_timeout_secs must be positive"
        );
        if let Some(url) = &self.url {
            ensure_field!(
                self.command.is_empty()
                    && self.args.is_empty()
                    && self.cwd.is_none()
                    && self.env.is_empty()
                    && self.env_from.is_empty(),
                "url",
                "url cannot be combined with command, args, cwd, env or env_from"
            );
            ensure_field!(
                !url.chars().any(char::is_control),
                "url",
                "url must be a valid HTTP endpoint"
            );
            let url = reqwest::Url::parse(url)
                .map_err(|_| FieldError::new("url", "url must be a valid HTTP endpoint"))?;
            ensure_field!(
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "url",
                "url must be HTTP(S) without credentials, query or fragment"
            );
            let loopback = url.host_str().is_some_and(|host| {
                host == "localhost"
                    || host.ends_with(".localhost")
                    || host
                        .trim_start_matches('[')
                        .trim_end_matches(']')
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|address| address.is_loopback())
            });
            ensure_field!(
                url.scheme() == "https" || loopback,
                "url",
                "url must use https unless it points at this machine (localhost/127.0.0.1/::1)"
            );
            ensure_field!(
                !self.oauth
                    || !self
                        .headers_from
                        .keys()
                        .any(|name| name.eq_ignore_ascii_case("authorization")),
                "headers_from",
                "oauth cannot be combined with an Authorization header in headers_from"
            );
        } else {
            ensure_field!(
                self.headers_from.is_empty(),
                "headers_from",
                "headers_from requires url"
            );
            ensure_field!(!self.oauth, "oauth", "oauth requires url");
        }
        let mut names = std::collections::BTreeSet::new();
        for (header, name) in &self.headers_from {
            ensure_field!(
                reqwest::header::HeaderName::from_bytes(header.as_bytes()).is_ok(),
                "headers_from",
                "headers_from must use valid HTTP header names"
            );
            let header = header.to_ascii_lowercase();
            ensure_field!(
                names.insert(header.clone()),
                "headers_from",
                "headers_from contains duplicate header names"
            );
            ensure_field!(
                !reserved_header(&header),
                "headers_from",
                "headers_from contains a reserved protocol header"
            );
            ensure_field!(
                valid_header_env(name)
                    && (["_API_KEY", "_KEY", "_TOKEN"]
                        .iter()
                        .any(|suffix| name.ends_with(suffix))
                        || header_key_envs.contains(name)),
                "headers_from",
                "headers_from values must be allowed environment variable identifiers"
            );
        }
        Ok(())
    }
}

/// Only '*' is special. Matching is case-sensitive and covers the whole raw name.
fn tool_pattern_matches(pattern: &str, name: &str) -> bool {
    let (pattern, name) = (pattern.as_bytes(), name.as_bytes());
    let (mut p, mut n) = (0, 0);
    let (mut star, mut retry) = (None, 0);
    while n < name.len() {
        if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            retry = n;
        } else if p < pattern.len() && pattern[p] == name[n] {
            p += 1;
            n += 1;
        } else if let Some(previous) = star {
            retry += 1;
            n = retry;
            p = previous + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

fn valid_header_env(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && name
            .bytes()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

pub(crate) fn reserved_header(name: &str) -> bool {
    matches!(
        name,
        "accept"
            | "content-type"
            | "host"
            | "content-length"
            | "connection"
            | "transfer-encoding"
            | "te"
            | "trailer"
            | "upgrade"
            | "proxy-connection"
            | "keep-alive"
            | "last-event-id"
    ) || name.starts_with("mcp-")
}

config_section! {
    #[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
    #[serde(default)]
    pub struct McpToolSettings {
        /// An omitted override inherits the server's policy.
        pub approval: Option<Approval> => LIVE,
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
