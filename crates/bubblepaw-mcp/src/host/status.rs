//! The settings page's view of each configured server: live state, sign-in
//! state and tools, plus change notifications. Never contains config values,
//! credentials or tokens.
use super::*;
use crate::oauth::ServerAuth;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerState {
    /// Connected and its tools listed.
    Running,
    /// Disabled in config.
    Stopped,
    /// Enabled but not serving; see `error`.
    Error,
    /// No longer configured (notifications only).
    Removed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpAuthState {
    /// The server does not use OAuth.
    None,
    NeedsSignIn,
    /// A sign-in was started and waits for its redirect or pasted URL.
    Waiting,
    SignedIn,
    /// Tokens were refused on refresh; sign in again.
    Expired,
    /// The last sign-in attempt failed; see `error`.
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct McpAuthStatus {
    pub state: McpAuthState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct McpToolStatus {
    /// The server's own tool name, as the include/exclude filters match it.
    pub name: String,
    /// False when `include_tools` / `exclude_tools` filter it out.
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct McpServerStatus {
    pub name: String,
    /// `stdio` or `http`.
    pub transport: &'static str,
    pub enabled: bool,
    pub state: McpServerState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub auth: McpAuthStatus,
    /// Tools the model sees from this server.
    pub tool_count: usize,
    /// Every tool the server lists, with its filter result. Empty unless running.
    pub tools: Vec<McpToolStatus>,
}

impl McpHost {
    /// One entry per configured server, sorted by name.
    pub fn servers(&self) -> Vec<McpServerStatus> {
        let state = self.inner.state.lock().unwrap();
        let auth = self.inner.auth.lock().unwrap();
        state
            .settings
            .servers
            .keys()
            .filter_map(|name| status(&state, &auth, name))
            .collect()
    }

    /// Server status changes: sign-in state, start, stop, failure and tool
    /// list changes. A lagging receiver should re-read `servers()`.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<McpServerStatus> {
        self.inner.status.subscribe()
    }
}

impl HostInner {
    /// Sends the status of every server whose view changed since it was last sent.
    pub(super) fn publish(&self) {
        let changed = {
            let state = self.state.lock().unwrap();
            let auth = self.auth.lock().unwrap();
            let mut sent = self.sent.lock().unwrap();
            let mut names: BTreeSet<String> = sent.keys().cloned().collect();
            names.extend(state.settings.servers.keys().cloned());
            let mut changed = Vec::new();
            for name in names {
                let current = status(&state, &auth, &name).unwrap_or_else(|| {
                    let mut gone = sent[&name].clone();
                    gone.state = McpServerState::Removed;
                    gone.error = None;
                    gone.tool_count = 0;
                    gone.tools.clear();
                    gone
                });
                if sent.get(&name) != Some(&current) {
                    if current.state == McpServerState::Removed {
                        sent.remove(&name);
                    } else {
                        sent.insert(name, current.clone());
                    }
                    changed.push(current);
                }
            }
            changed
        };
        for status in changed {
            let _ = self.status.send(status);
        }
    }
}

fn status(
    state: &HostState,
    auth: &BTreeMap<String, Arc<ServerAuth>>,
    name: &str,
) -> Option<McpServerStatus> {
    let config = state.settings.servers.get(name)?;
    let auth = match (config.oauth && config.url.is_some(), auth.get(name)) {
        (true, Some(auth)) => {
            let (state, error) = auth.status();
            McpAuthStatus { state, error }
        }
        (true, None) => McpAuthStatus {
            state: McpAuthState::NeedsSignIn,
            error: None,
        },
        (false, _) => McpAuthStatus {
            state: McpAuthState::None,
            error: None,
        },
    };
    let entry = state.servers.get(name);
    let (server_state, error) = match (config.enabled, entry) {
        (false, _) => (McpServerState::Stopped, None),
        (true, Some(_)) => (McpServerState::Running, None),
        (true, None) => (
            McpServerState::Error,
            Some(
                state
                    .failures
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| "not started".into()),
            ),
        ),
    };
    let tools: Vec<McpToolStatus> = entry
        .filter(|_| config.enabled)
        .map(|entry| {
            entry
                .listed
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .map(|tool| McpToolStatus {
                    name: tool.to_owned(),
                    enabled: entry.config.allows_tool(tool),
                })
                .collect()
        })
        .unwrap_or_default();
    let tool_count = state
        .tools
        .values()
        .filter(|tool| tool.server_name == name)
        .count();
    Some(McpServerStatus {
        name: name.to_owned(),
        transport: if config.url.is_some() {
            "http"
        } else {
            "stdio"
        },
        enabled: config.enabled,
        state: server_state,
        error,
        auth,
        tool_count,
        tools,
    })
}
