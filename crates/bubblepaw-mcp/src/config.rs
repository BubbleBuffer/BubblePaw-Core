use anyhow::{Result, ensure};
use bubblepaw_core::Approval;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt, path::PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct McpSettings {
    pub header_key_envs: Vec<String>,
    pub servers: BTreeMap<String, McpServerSettings>,
}

/// Secret values are resolved only at connection time, never during config rendering.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct McpServerSettings {
    pub command: String,
    pub url: Option<String>,
    pub headers_from: BTreeMap<String, String>,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub env_from: BTreeMap<String, String>,
    pub enabled: bool,
    /// Discover on demand instead of declaring every server tool to the model.
    /// Set false for servers whose tools should always be native declarations.
    pub defer: bool,
    pub approval: Approval,
    pub startup_timeout_secs: u64,
    pub call_timeout_secs: u64,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    /// Optional allowlist overriding legacy include. Some(empty) permits none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_tools: Option<Vec<String>>,
    pub exclude_tools: Vec<String>,
    pub tools: BTreeMap<String, McpToolSettings>,
}

impl Default for McpServerSettings {
    fn default() -> Self {
        Self {
            command: String::new(),
            url: None,
            headers_from: BTreeMap::new(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            env_from: BTreeMap::new(),
            enabled: true,
            defer: true,
            approval: Approval::Confirm,
            startup_timeout_secs: 30,
            call_timeout_secs: 120,
            include: Vec::new(),
            exclude: Vec::new(),
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
            .field("enabled", &self.enabled)
            .field("defer", &self.defer)
            .field("approval", &self.approval)
            .field("startup_timeout_secs", &self.startup_timeout_secs)
            .field("call_timeout_secs", &self.call_timeout_secs)
            .field("include", &self.include)
            .field("exclude", &self.exclude)
            .field("include_tools", &self.include_tools)
            .field("exclude_tools", &self.exclude_tools)
            .field("tools", &self.tools)
            .finish()
    }
}

impl McpServerSettings {
    pub(crate) fn allows_tool(&self, name: &str) -> bool {
        let included = match &self.include_tools {
            Some(patterns) => patterns
                .iter()
                .any(|pattern| tool_pattern_matches(pattern, name)),
            None => {
                self.include.is_empty()
                    || self
                        .include
                        .iter()
                        .any(|pattern| tool_pattern_matches(pattern, name))
            }
        };
        included
            && !self
                .exclude
                .iter()
                .chain(&self.exclude_tools)
                .any(|pattern| tool_pattern_matches(pattern, name))
    }

    /// Inventory filters and deferral are host settings and need no new transport.
    pub(crate) fn same_connection_settings(&self, other: &Self) -> bool {
        let mut current = self.clone();
        current.defer = other.defer;
        current.include = other.include.clone();
        current.exclude = other.exclude.clone();
        current.include_tools = other.include_tools.clone();
        current.exclude_tools = other.exclude_tools.clone();
        current == *other
    }

    /// Validate before resolving credentials or opening either transport.
    /// Diagnostics name fields only, including for malformed secret references.
    pub fn validate(&self, header_key_envs: &[String]) -> Result<()> {
        ensure!(
            header_key_envs.iter().all(|name| valid_header_env(name)),
            "header_key_envs must contain environment variable identifiers"
        );
        let command = !self.command.trim().is_empty();
        ensure!(
            command != self.url.is_some(),
            "exactly one of command or url is required"
        );
        ensure!(
            self.startup_timeout_secs > 0,
            "startup_timeout_secs must be positive"
        );
        ensure!(
            self.call_timeout_secs > 0,
            "call_timeout_secs must be positive"
        );
        if let Some(url) = &self.url {
            ensure!(
                self.command.is_empty()
                    && self.args.is_empty()
                    && self.cwd.is_none()
                    && self.env.is_empty()
                    && self.env_from.is_empty(),
                "url cannot be combined with command, args, cwd, env or env_from"
            );
            ensure!(
                !url.chars().any(char::is_control),
                "url must be a valid HTTP endpoint"
            );
            let url = reqwest::Url::parse(url)
                .map_err(|_| anyhow::anyhow!("url must be a valid HTTP endpoint"))?;
            ensure!(
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
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
            ensure!(
                url.scheme() == "https" || loopback,
                "url must use https unless it points at this machine (localhost/127.0.0.1/::1)"
            );
        } else {
            ensure!(self.headers_from.is_empty(), "headers_from requires url");
        }
        let mut names = std::collections::BTreeSet::new();
        for (header, name) in &self.headers_from {
            ensure!(
                reqwest::header::HeaderName::from_bytes(header.as_bytes()).is_ok(),
                "headers_from must use valid HTTP header names"
            );
            let header = header.to_ascii_lowercase();
            ensure!(
                names.insert(header.clone()),
                "headers_from contains duplicate header names"
            );
            ensure!(
                !reserved_header(&header),
                "headers_from contains a reserved protocol header"
            );
            ensure!(
                valid_header_env(name)
                    && (["_API_KEY", "_KEY", "_TOKEN"]
                        .iter()
                        .any(|suffix| name.ends_with(suffix))
                        || header_key_envs.contains(name)),
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

fn reserved_header(name: &str) -> bool {
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct McpToolSettings {
    /// An omitted override inherits the server's policy.
    pub approval: Option<Approval>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_defer_by_default_and_core_policy_needs_no_new_transport() {
        let deferred: McpServerSettings =
            serde_json::from_value(serde_json::json!({"command": "fixture"})).unwrap();
        assert!(deferred.defer);
        assert!(McpServerSettings::default().defer);
        let core: McpServerSettings = serde_json::from_value(serde_json::json!({
            "command": "fixture", "defer": false
        }))
        .unwrap();
        assert!(!core.defer);
        assert!(deferred.same_connection_settings(&core));
        assert_ne!(deferred, core);
        let encoded = serde_json::to_value(core).unwrap();
        assert_eq!(encoded["defer"], false);
    }

    #[test]
    fn tool_filters_distinguish_omitted_empty_and_legacy_include() {
        let mut settings: McpServerSettings = serde_json::from_value(serde_json::json!({
            "command": "fixture", "include": ["read_*"], "exclude": ["*_secret"]
        }))
        .unwrap();
        assert!(settings.allows_tool("read_note"));
        assert!(!settings.allows_tool("read_secret"));
        assert!(!settings.allows_tool("echo"));
        settings.include_tools = Some(vec!["echo".into(), "read_*".into()]);
        settings.exclude_tools = vec!["read_note".into()];
        assert!(settings.allows_tool("echo"));
        assert!(!settings.allows_tool("read_note"));
        assert!(!settings.allows_tool("read_secret"));
        settings.include_tools = Some(Vec::new());
        assert!(!settings.allows_tool("echo"));
        let encoded = serde_json::to_value(&settings).unwrap();
        assert_eq!(encoded["include_tools"], serde_json::json!([]));
        let decoded: McpServerSettings = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.include_tools, Some(Vec::new()));
        settings.include_tools = None;
        settings.include.clear();
        settings.exclude.clear();
        settings.exclude_tools.clear();
        assert!(settings.allows_tool("anything"));
        assert!(
            serde_json::to_value(&settings)
                .unwrap()
                .get("include_tools")
                .is_none()
        );
    }

    #[test]
    fn tool_globs_match_raw_whole_names_with_only_star_special() {
        for (pattern, name, expected) in [
            ("read", "read", true),
            ("read", "read_note", false),
            ("read_*", "read_note", true),
            ("*note", "read_note", true),
            ("*read*note*", "my_read_a_note_v2", true),
            ("a**b*c", "axbybc", true),
            ("*", "猫の道具", true),
            ("猫*具", "猫の道具", true),
            ("a.*", "a.b", true),
            ("a.*", "a_b", false),
            ("read?", "readx", false),
            ("READ*", "read_note", false),
            ("*read", "read_note", false),
            ("", "", true),
            ("*", "", true),
        ] {
            assert_eq!(
                tool_pattern_matches(pattern, name),
                expected,
                "{pattern}: {name}"
            );
        }
    }

    fn http(url: &str) -> McpServerSettings {
        McpServerSettings {
            url: Some(url.into()),
            ..Default::default()
        }
    }

    #[test]
    fn http_url_rules_match_provider_setup() {
        for url in [
            "https://example.com/mcp",
            "http://localhost/mcp",
            "http://dev.localhost:9000/mcp",
            "http://127.12.34.56/mcp",
            "http://[::1]/mcp",
        ] {
            assert!(http(url).validate(&[]).is_ok(), "{url}");
        }
        for url in [
            "http://example.com/mcp",
            "http://192.168.1.1/mcp",
            "http://localhost.example.com/mcp",
            "http://[::2]/mcp",
            "ftp://localhost/mcp",
            "https://user:synthetic-secret@example.com/mcp",
            "https://example.com/mcp?key=synthetic-secret",
            "https://example.com/mcp#synthetic-secret",
            "https://example.com/\nsynthetic-secret",
            "synthetic-secret",
        ] {
            let error = http(url).validate(&[]).unwrap_err().to_string();
            assert!(!error.contains("synthetic-secret"));
        }
    }

    #[test]
    fn transports_are_exclusive_and_stdio_defaults_remain_valid() {
        assert!(McpServerSettings::default().validate(&[]).is_err());
        let stdio = McpServerSettings {
            command: "server".into(),
            ..Default::default()
        };
        assert!(stdio.validate(&[]).is_ok());
        let mut config = http("https://example.com/mcp");
        config.command = "server".into();
        assert!(config.validate(&[]).is_err());
        config.command.clear();
        config.args.push("arg".into());
        assert!(config.validate(&[]).is_err());
        config.args.clear();
        config.cwd = Some(".".into());
        assert!(config.validate(&[]).is_err());
        config.cwd = None;
        config.env.insert("LOG_LEVEL".into(), "warn".into());
        assert!(config.validate(&[]).is_err());
        config.env.clear();
        config.env_from.insert("TOKEN".into(), "HOST_TOKEN".into());
        assert!(config.validate(&[]).is_err());
        let mut stdio = stdio;
        stdio
            .headers_from
            .insert("authorization".into(), "HOST_TOKEN".into());
        assert!(stdio.validate(&[]).is_err());
    }

    #[test]
    fn http_headers_require_env_names_and_reserve_protocol_headers() {
        let mut config = http("https://example.com/mcp");
        for name in ["MCP_API_KEY", "MCP_KEY", "MCP_TOKEN"] {
            config
                .headers_from
                .insert("Authorization".into(), name.into());
            assert!(config.validate(&[]).is_ok());
        }
        for name in [
            "CUSTOM_AUTH",
            "Bearer synthetic-secret",
            "1_BAD_TOKEN",
            "BAD-TOKEN",
            "",
            "BAD\n_TOKEN",
        ] {
            config
                .headers_from
                .insert("Authorization".into(), name.into());
            let error = config.validate(&[]).unwrap_err().to_string();
            assert!(!error.contains("synthetic-secret"));
        }
        config
            .headers_from
            .insert("Authorization".into(), "CUSTOM_AUTH".into());
        assert!(config.validate(&["CUSTOM_AUTH".into()]).is_ok());
        assert!(config.validate(&["bad identifier".into()]).is_err());
        config.headers_from.clear();
        for header in [
            "Accept",
            "Content-Type",
            "Host",
            "Content-Length",
            "MCP-Protocol-Version",
            "Mcp-Session-Id",
            "Last-Event-ID",
            "Mcp-Method",
            "Mcp-Name",
            "Mcp-Param-Test",
            "Connection",
            "Transfer-Encoding",
            "bad header",
            "X-\r\nsecret",
        ] {
            config
                .headers_from
                .insert(header.into(), "MCP_TOKEN".into());
            assert!(config.validate(&[]).is_err(), "{header}");
            config.headers_from.clear();
        }
        config
            .headers_from
            .insert("Authorization".into(), "MCP_TOKEN".into());
        config
            .headers_from
            .insert("authorization".into(), "MCP_TOKEN".into());
        assert!(config.validate(&[]).is_err());
    }

    #[test]
    fn debug_omits_invalid_secret_references_and_url() {
        let mut config = http("https://user:synthetic-secret@example.com/mcp");
        config
            .headers_from
            .insert("Authorization".into(), "Bearer synthetic-secret".into());
        config
            .env_from
            .insert("CHILD".into(), "Bearer synthetic-secret".into());
        assert!(!format!("{config:?}").contains("synthetic-secret"));
    }
}
