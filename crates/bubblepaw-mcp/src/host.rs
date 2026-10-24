use crate::{McpClientEnv, McpServerSettings, McpSettings, transport::Connection};
use anyhow::{Result, anyhow, bail, ensure};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Tool, ToolContext, ToolDefinition, ToolRegistry, ToolResult,
};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::task::JoinSet;

mod registry;

/// Ordered inventory changes. Tool fields use registered names; lifecycle
/// fields use configured server names. Diagnostics never contain config values.
#[derive(Debug, Clone, Default, Serialize)]
pub struct McpDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
    pub started: Vec<String>,
    pub stopped: Vec<String>,
    pub restarted: Vec<String>,
    pub filtered: Vec<String>,
    pub warnings: Vec<String>,
}
impl McpDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.changed.is_empty()
            && self.started.is_empty()
            && self.stopped.is_empty()
            && self.restarted.is_empty()
            && self.filtered.is_empty()
            && self.warnings.is_empty()
    }
}

/// Shared host inventory. Calls pin generations; retained tool handles do not.
pub struct McpHost {
    inner: Arc<HostInner>,
}
struct HostInner {
    workspace: PathBuf,
    /// Retained for the host's life: startup, reload and reconnect all resolve from it.
    client_env: Option<Arc<McpClientEnv>>,
    state: Mutex<HostState>,
    gate: tokio::sync::Mutex<()>,
    reapers: Mutex<JoinSet<()>>,
    retired: Mutex<Vec<Weak<Server>>>,
    closed: AtomicBool,
}
#[derive(Default)]
struct HostState {
    servers: BTreeMap<String, Entry>,
    tools: BTreeMap<String, Registered>,
    removed: BTreeMap<String, String>,
    warnings: Vec<String>,
}
struct Entry {
    server: Arc<Server>,
    config: McpServerSettings,
    listed: Vec<Value>,
}
#[derive(Clone)]
struct Registered {
    server_name: String,
    server: Arc<Server>,
    original_name: String,
    definition: ToolDefinition,
}
impl McpHost {
    /// Standalone startup: credentials and child env come from this process.
    pub async fn start(settings: &McpSettings, workspace: &Path) -> Self {
        Self::start_inner(settings, workspace, None).await
    }

    /// Startup with a request-scoped client snapshot. `env_from` and
    /// `headers_from` resolve only from it, and stdio children inherit the host
    /// env with the snapshot overlaid. The snapshot is retained for reload and
    /// reconnect, and is never shared with another host.
    pub async fn start_with_client_env(
        settings: &McpSettings,
        workspace: &Path,
        client_env: McpClientEnv,
    ) -> Self {
        Self::start_inner(settings, workspace, Some(Arc::new(client_env))).await
    }

    async fn start_inner(
        settings: &McpSettings,
        workspace: &Path,
        client_env: Option<Arc<McpClientEnv>>,
    ) -> Self {
        let host = Self {
            inner: Arc::new(HostInner {
                workspace: workspace.to_owned(),
                client_env,
                state: Mutex::default(),
                gate: tokio::sync::Mutex::new(()),
                reapers: Mutex::new(JoinSet::new()),
                retired: Mutex::default(),
                closed: AtomicBool::new(false),
            }),
        };
        if let Err(error) = host.reload(settings, None).await {
            host.inner
                .state
                .lock()
                .unwrap()
                .warnings
                .push(error.to_string());
        }
        host
    }

    /// Validate first, then replace changed entries. Startup failures omit that
    /// server's tools and warn; unchanged servers continue serving. A forced
    /// name must exist and be enabled. This never replays a dispatched call.
    pub async fn reload(&self, settings: &McpSettings, force: Option<&str>) -> Result<McpDiff> {
        let _gate = self.inner.gate.lock().await;
        ensure!(
            !self.inner.closed.load(Ordering::SeqCst),
            "MCP host shut down"
        );
        for config in settings.servers.values().filter(|c| c.enabled) {
            config.validate(&settings.header_key_envs)?;
        }
        if let Some(name) = force {
            ensure!(
                settings.servers.get(name).is_some_and(|c| c.enabled),
                "MCP server {name} is not configured and enabled"
            );
        }
        let mut jobs = JoinSet::new();
        let mut filter_updates = BTreeMap::new();
        {
            let state = self.inner.state.lock().unwrap();
            for (name, config) in &settings.servers {
                if !config.enabled {
                    continue;
                }
                if force != Some(name.as_str())
                    && let Some(entry) = state.servers.get(name)
                    && entry.config.same_connection_settings(config)
                {
                    if entry.config != *config {
                        filter_updates.insert(name.clone(), config.clone());
                    }
                    continue;
                }
                let name = name.clone();
                let server = Arc::new(Server::new(
                    config.clone(),
                    settings.header_key_envs.clone(),
                    self.inner.workspace.clone(),
                    self.inner.client_env.clone(),
                ));
                jobs.spawn(async move {
                    let result = tokio::time::timeout(
                        Duration::from_secs(server.config.startup_timeout_secs),
                        server.initial_tools(),
                    )
                    .await;
                    let result = match result {
                        Ok(Ok(tools)) => Ok(tools),
                        Ok(Err(e)) => Err(e.to_string()),
                        Err(_) => Err("startup timed out".into()),
                    };
                    if result.is_err() {
                        // This server was never published. Finish its existing
                        // owned-child cleanup before snapshotting stderr below.
                        server.shutdown(Duration::ZERO).await;
                    }
                    (name, server, result)
                });
            }
        }
        let mut discovered = BTreeMap::new();
        while let Some(result) = jobs.join_next().await {
            let (name, server, result) = result.map_err(|_| anyhow!("MCP startup task failed"))?;
            discovered.insert(name, (server, result));
        }
        let mut diff = McpDiff::default();
        let mut retired = Vec::new();
        {
            let mut state = self.inner.state.lock().unwrap();
            let before = definitions(&state);
            let removed: Vec<_> = state
                .servers
                .keys()
                .filter(|name| !settings.servers.get(*name).is_some_and(|c| c.enabled))
                .cloned()
                .collect();
            for name in removed {
                retired.push(state.servers.remove(&name).unwrap().server);
                mark_removed(&mut state, &name, "configuration was removed or disabled");
                diff.stopped.push(name);
            }
            for (name, (server, result)) in discovered {
                if let Some(old) = state.servers.remove(&name) {
                    retired.push(old.server);
                    diff.restarted.push(name.clone());
                } else {
                    diff.started.push(name.clone());
                }
                match result {
                    Ok(listed) => {
                        let config = server.config.clone();
                        state.servers.insert(
                            name,
                            Entry {
                                server,
                                config,
                                listed,
                            },
                        );
                    }
                    Err(error) => {
                        let detail = server
                            .connection
                            .lock()
                            .unwrap()
                            .as_ref()
                            .map(|c| c.stderr())
                            .unwrap_or_default();
                        let suffix = if detail.trim().is_empty() {
                            String::new()
                        } else {
                            format!("; stderr: {}", detail.trim())
                        };
                        mark_removed(&mut state, &name, "replacement failed to start");
                        diff.warnings
                            .push(format!("MCP server {name}: {error}{suffix}"));
                        retired.push(server);
                    }
                }
            }
            for (name, config) in filter_updates {
                state.servers.get_mut(&name).unwrap().config = config;
                diff.filtered.push(name);
            }
            rebuild(&mut state, &mut diff.warnings);
            inventory_diff(&before, &mut state, &mut diff);
            state.warnings = diff.warnings.clone();
        }
        for server in retired {
            self.retire(server);
        }
        self.reap_finished();
        Ok(diff)
    }

    /// Poll at turn/reload boundaries even if no config file changed. Failed
    /// refreshes keep the validated inventory and leave the notification pending.
    pub async fn refresh_tools(&self) -> McpDiff {
        let _gate = self.inner.gate.lock().await;
        if self.inner.closed.load(Ordering::SeqCst) {
            return McpDiff::default();
        }
        let servers: Vec<_> = self
            .inner
            .state
            .lock()
            .unwrap()
            .servers
            .iter()
            .map(|(n, e)| (n.clone(), e.server.clone()))
            .collect();
        let mut refreshed = BTreeMap::new();
        let mut guards = Vec::new();
        let mut diff = McpDiff::default();
        for (name, server) in servers {
            let pending = server.refreshed.lock().unwrap().clone();
            if let Some(listed) = pending {
                refreshed.insert(name, listed);
                continue;
            }
            let connection = server.connection.lock().unwrap().clone();
            let Some(connection) = connection else {
                continue;
            };
            if !connection.take_tools_changed() {
                continue;
            }
            let dirty = RefreshGuard {
                connection: connection.clone(),
                published: false,
            };
            match tokio::time::timeout(
                Duration::from_secs(server.config.startup_timeout_secs),
                Server::list_tools(&connection),
            )
            .await
            {
                Ok(Ok(listed)) => {
                    refreshed.insert(name, listed);
                    guards.push(dirty);
                }
                result => {
                    let reason = match result {
                        Ok(Err(e)) => connection.redact(&e.to_string()),
                        _ => "tool-list refresh timed out".into(),
                    };
                    diff.warnings.push(format!("MCP server {name}: {reason}"));
                }
            }
        }
        if !refreshed.is_empty() {
            let mut state = self.inner.state.lock().unwrap();
            let before = definitions(&state);
            for (name, listed) in refreshed {
                if let Some(entry) = state.servers.get_mut(&name) {
                    let mut pending = entry.server.refreshed.lock().unwrap();
                    if pending.as_ref() == Some(&listed) {
                        pending.take();
                    }
                    entry.listed = listed;
                }
            }
            rebuild(&mut state, &mut diff.warnings);
            inventory_diff(&before, &mut state, &mut diff);
            state.warnings = diff.warnings.clone();
        }
        for guard in &mut guards {
            guard.published = true;
        }
        self.reap_finished();
        diff
    }
    pub fn warnings(&self) -> Vec<String> {
        self.inner.state.lock().unwrap().warnings.clone()
    }
    fn retire(&self, server: Arc<Server>) {
        self.inner
            .retired
            .lock()
            .unwrap()
            .push(Arc::downgrade(&server));
        server.retired.store(true, Ordering::SeqCst);
        if server.active.load(Ordering::SeqCst) == 0 {
            server.drained.cancel();
        }
        self.inner.reapers.lock().unwrap().spawn(async move {
            server.drained.cancelled().await;
            server.shutdown(Duration::from_secs(2)).await;
        });
    }
    fn reap_finished(&self) {
        self.inner
            .retired
            .lock()
            .unwrap()
            .retain(|server| server.strong_count() > 0);
        let mut reapers = self.inner.reapers.lock().unwrap();
        while reapers.try_join_next().is_some() {}
    }
    /// Explicit shutdown may cancel calls. Generation replacement never does.
    pub async fn shutdown(&self, grace: Duration) {
        let _gate = self.inner.gate.lock().await;
        self.inner.closed.store(true, Ordering::SeqCst);
        let mut servers: Vec<_> = self
            .inner
            .state
            .lock()
            .unwrap()
            .servers
            .values()
            .map(|e| e.server.clone())
            .collect();
        servers.extend(
            self.inner
                .retired
                .lock()
                .unwrap()
                .iter()
                .filter_map(Weak::upgrade),
        );
        let mut jobs = JoinSet::new();
        for server in servers {
            jobs.spawn(async move { server.shutdown(grace).await });
        }
        while jobs.join_next().await.is_some() {}
        let mut reapers = std::mem::take(&mut *self.inner.reapers.lock().unwrap());
        if tokio::time::timeout(grace, async {
            while reapers.join_next().await.is_some() {}
        })
        .await
        .is_err()
        {
            reapers.abort_all();
            while reapers.join_next().await.is_some() {}
        }
    }
}
impl Drop for McpHost {
    fn drop(&mut self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        for entry in self.inner.state.lock().unwrap().servers.values() {
            entry.server.stop();
        }
        for server in self
            .inner
            .retired
            .lock()
            .unwrap()
            .iter()
            .filter_map(Weak::upgrade)
        {
            server.stop();
        }
        self.inner.reapers.lock().unwrap().abort_all();
    }
}
fn definitions(state: &HostState) -> BTreeMap<String, Value> {
    state
        .tools
        .iter()
        .map(|(n, t)| (n.clone(), serde_json::to_value(&t.definition).unwrap()))
        .collect()
}
fn inventory_diff(before: &BTreeMap<String, Value>, state: &mut HostState, diff: &mut McpDiff) {
    let after = definitions(state);
    diff.added = after
        .keys()
        .filter(|n| !before.contains_key(*n))
        .cloned()
        .collect();
    diff.removed = before
        .keys()
        .filter(|n| !after.contains_key(*n))
        .cloned()
        .collect();
    diff.changed = after
        .iter()
        .filter(|(n, d)| before.get(*n).is_some_and(|old| old != *d))
        .map(|(n, _)| n.clone())
        .collect();
    for name in &diff.removed {
        state.removed.entry(name.clone()).or_insert_with(|| {
            "tool removed from the current server list or configuration filter".into()
        });
    }
    for name in after.keys() {
        state.removed.remove(name);
    }
}
fn mark_removed(state: &mut HostState, server_name: &str, reason: &str) {
    for (name, tool) in &state.tools {
        if tool.server_name == server_name {
            state
                .removed
                .insert(name.clone(), format!("server {server_name}: {reason}"));
        }
    }
}
fn rebuild(state: &mut HostState, warnings: &mut Vec<String>) {
    let mut tools = BTreeMap::new();
    let mut registry = ToolRegistry::new();
    for (name, entry) in &state.servers {
        let server = &entry.server;
        for listed in &entry.listed {
            let Some(original_name) = listed["name"].as_str() else {
                warnings.push(format!("MCP server {name}: skipped tool without a name"));
                continue;
            };
            if !entry.config.allows_tool(original_name) {
                continue;
            }
            let registered = tool_name(name, original_name);
            if tools.contains_key(&registered) {
                warnings.push(format!(
                    "MCP tool {registered}: skipped duplicate registered name"
                ));
                continue;
            }
            let Some(schema) = listed.get("inputSchema") else {
                warnings.push(format!(
                    "MCP tool {registered}: skipped missing input schema"
                ));
                continue;
            };
            let connection = server.connection.lock().unwrap().clone();
            if connection
                .as_ref()
                .is_some_and(|c| c.register_tool(original_name, schema).is_err())
            {
                warnings.push(format!(
                    "MCP tool {registered}: skipped invalid HTTP header annotation"
                ));
                continue;
            }
            let approval = entry
                .config
                .tools
                .get(original_name)
                .and_then(|t| t.approval)
                .unwrap_or(entry.config.approval);
            let definition = ToolDefinition {
                name: registered.clone(),
                description: listed["description"].as_str().unwrap_or("").into(),
                input_schema: schema.clone(),
                read_only: false,
                approval,
            };
            // Compile before publishing; compiler errors may contain remote
            // contents, so only a fixed diagnostic escapes.
            if registry
                .register(Arc::new(McpTool {
                    host: Weak::new(),
                    definition: definition.clone(),
                }))
                .is_err()
            {
                warnings.push(format!(
                    "MCP tool {registered}: skipped invalid input schema"
                ));
                continue;
            }
            tools.insert(
                registered,
                Registered {
                    server_name: name.clone(),
                    server: server.clone(),
                    original_name: original_name.into(),
                    definition,
                },
            );
        }
    }
    state.tools = tools;
}
/// A cancelled refresh must leave the notification pending. Success is
/// acknowledged only after every collected list is published without awaits.
struct RefreshGuard {
    connection: Arc<Connection>,
    published: bool,
}
impl Drop for RefreshGuard {
    fn drop(&mut self) {
        if !self.published {
            self.connection.mark_tools_changed();
        }
    }
}
struct Server {
    config: McpServerSettings,
    allowed_header_envs: Vec<String>,
    workspace: PathBuf,
    client_env: Option<Arc<McpClientEnv>>,
    connection: Mutex<Option<Arc<Connection>>>,
    gate: tokio::sync::Mutex<()>,
    closed: AtomicBool,
    ready: AtomicBool,
    stopping: CancellationToken,
    active: AtomicUsize,
    retired: AtomicBool,
    drained: CancellationToken,
    refreshed: Mutex<Option<Vec<Value>>>,
}
impl Server {
    fn new(
        config: McpServerSettings,
        allowed_header_envs: Vec<String>,
        workspace: PathBuf,
        client_env: Option<Arc<McpClientEnv>>,
    ) -> Self {
        Self {
            config,
            allowed_header_envs,
            workspace,
            client_env,
            connection: Mutex::default(),
            gate: tokio::sync::Mutex::new(()),
            closed: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            stopping: CancellationToken::new(),
            active: AtomicUsize::new(0),
            retired: AtomicBool::new(false),
            drained: CancellationToken::new(),
            refreshed: Mutex::default(),
        }
    }
    async fn establish(&self) -> Result<Arc<Connection>> {
        {
            let _slot = self.connection.lock().unwrap();
            ensure!(!self.closed.load(Ordering::SeqCst), "MCP host shut down");
            self.ready.store(false, Ordering::SeqCst);
        }
        // Opening validates config and spawns the stdio server synchronously,
        // so it runs on the blocking pool.
        let (config, workspace, envs, client_env) = (
            self.config.clone(),
            self.workspace.clone(),
            self.allowed_header_envs.clone(),
            self.client_env.clone(),
        );
        let connection = tokio::task::spawn_blocking(move || {
            Connection::open(&config, &workspace, &envs, client_env.as_deref())
        })
        .await??;
        {
            let mut slot = self.connection.lock().unwrap();
            if self.closed.load(Ordering::SeqCst) {
                connection.stop(Duration::ZERO);
                bail!("MCP host shut down");
            }
            *slot = Some(connection.clone());
        }
        let mut guard = EstablishGuard {
            connection: connection.clone(),
            ready: false,
        };
        tokio::select! {
            biased;
            _ = self.stopping.cancelled() => bail!("MCP host shut down"),
            result = connection.initialize() => result?,
        }
        guard.ready = true;
        Ok(connection)
    }
    async fn initial_tools(&self) -> Result<Vec<Value>> {
        let connection = self.establish().await?;
        let tools = Self::list_tools(&connection).await?;
        self.ready.store(true, Ordering::SeqCst);
        Ok(tools)
    }
    async fn list_tools(connection: &Arc<Connection>) -> Result<Vec<Value>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        let mut cursors = BTreeSet::new();
        for _ in 0..100 {
            let params = cursor
                .as_ref()
                .map(|cursor| json!({"cursor":cursor}))
                .unwrap_or_else(|| json!({}));
            let result = connection.request("tools/list", params).await?;
            ensure!(
                result
                    .get("resultType")
                    .is_none_or(|kind| kind == "complete"),
                "unsupported MCP tool-list result type"
            );
            let page = result["tools"]
                .as_array()
                .ok_or_else(|| anyhow!("invalid MCP tool list"))?;
            tools.extend(page.iter().cloned());
            match result.get("nextCursor") {
                None | Some(Value::Null) => return Ok(tools),
                Some(Value::String(next)) => {
                    ensure!(
                        cursors.insert(next.clone()),
                        "MCP tool pagination repeated a cursor"
                    );
                    cursor = Some(next.clone());
                }
                _ => bail!("invalid MCP tool pagination cursor"),
            }
        }
        bail!("MCP tool pagination exceeds 100 pages")
    }
    async fn connected(&self) -> Result<Arc<Connection>> {
        let _gate = self.gate.lock().await;
        ensure!(!self.closed.load(Ordering::SeqCst), "MCP host shut down");
        let previous = self.connection.lock().unwrap().clone();
        if let Some(connection) = &previous
            && connection.alive()
            && self.ready.load(Ordering::SeqCst)
        {
            return Ok(connection.clone());
        }
        if let Some(connection) = previous {
            connection.shutdown(Duration::ZERO).await;
        }
        // Discover again for the next call. Never resend the interrupted effect.
        let result = tokio::time::timeout(
            Duration::from_secs(self.config.startup_timeout_secs),
            async {
                let listed = self.initial_tools().await?;
                *self.refreshed.lock().unwrap() = Some(listed);
                Ok::<_, anyhow::Error>(self.connection.lock().unwrap().as_ref().unwrap().clone())
            },
        )
        .await;
        match result {
            Ok(Ok(connection)) => Ok(connection),
            result => {
                let failed = self.connection.lock().unwrap().take();
                if let Some(connection) = failed {
                    connection.shutdown(Duration::ZERO).await;
                }
                match result {
                    Ok(Err(error)) => Err(error),
                    _ => bail!("MCP restart timed out"),
                }
            }
        }
    }
    fn stop(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.stopping.cancel();
        if let Some(connection) = self.connection.lock().unwrap().as_ref() {
            connection.stop(Duration::ZERO);
        }
    }
    async fn shutdown(&self, grace: Duration) {
        self.closed.store(true, Ordering::SeqCst);
        self.stopping.cancel();
        let connection = self.connection.lock().unwrap().clone();
        if let Some(connection) = connection {
            connection.shutdown(grace).await;
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}
struct Lease(Arc<Server>);
impl Drop for Lease {
    fn drop(&mut self) {
        if self.0.active.fetch_sub(1, Ordering::SeqCst) == 1
            && self.0.retired.load(Ordering::SeqCst)
        {
            self.0.drained.cancel();
        }
    }
}
struct EstablishGuard {
    connection: Arc<Connection>,
    ready: bool,
}
impl Drop for EstablishGuard {
    fn drop(&mut self) {
        if !self.ready {
            self.connection.stop(Duration::ZERO);
        }
    }
}
struct McpTool {
    host: Weak<HostInner>,
    definition: ToolDefinition,
}
#[async_trait]
impl Tool for McpTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }
    async fn invoke(&self, context: ToolContext, input: Value) -> Result<ToolResult> {
        let Some(host) = self.host.upgrade() else {
            return Ok(ToolResult::error("MCP host shut down; call was not sent"));
        };
        let (registered, _lease) = {
            let state = host.state.lock().unwrap();
            if host.closed.load(Ordering::SeqCst) {
                return Ok(ToolResult::error("MCP host shut down; call was not sent"));
            }
            let Some(registered) = state.tools.get(&self.definition.name) else {
                let reason = state
                    .removed
                    .get(&self.definition.name)
                    .map(String::as_str)
                    .unwrap_or("tool unavailable in the current inventory");
                return Ok(ToolResult::error(format!(
                    "MCP tool {} removed: {reason}; call was not sent",
                    self.definition.name
                )));
            };
            if serde_json::to_value(&registered.definition)?
                != serde_json::to_value(&self.definition)?
            {
                return Ok(ToolResult::error(format!(
                    "MCP tool {} definition changed; refresh the tool registry before calling; call was not sent",
                    self.definition.name
                )));
            }
            registered.server.active.fetch_add(1, Ordering::SeqCst);
            (registered.clone(), Lease(registered.server.clone()))
        };
        let connected = tokio::select! {
            biased;
            _ = context.cancellation.cancelled() => bail!("Cancelled before MCP call; outcome unknown"),
            result = registered.server.connected() => result,
        };
        let connection = match connected {
            Ok(connection) => connection,
            Err(error) => {
                return Ok(ToolResult::error(format!(
                    "MCP server unavailable: {error}; call was not sent"
                )));
            }
        };
        // Reconnect discovery is published at the next host boundary. Reject
        // removed or changed contracts before then rather than dispatching with
        // stale validation, header annotations, or approval metadata.
        if let Some(listed) = registered.server.refreshed.lock().unwrap().as_ref() {
            let Some(current) = listed
                .iter()
                .find(|tool| tool["name"] == registered.original_name)
            else {
                return Ok(ToolResult::error(format!(
                    "MCP tool {} removed by server reconnect; call was not sent",
                    self.definition.name
                )));
            };
            if current.get("inputSchema") != Some(&registered.definition.input_schema)
                || current["description"].as_str().unwrap_or("")
                    != registered.definition.description
            {
                return Ok(ToolResult::error(format!(
                    "MCP tool {} definition changed after server reconnect; refresh the tool registry; call was not sent",
                    self.definition.name
                )));
            }
        }
        if connection
            .register_tool(
                &registered.original_name,
                &registered.definition.input_schema,
            )
            .is_err()
        {
            return Ok(ToolResult::error(
                "Invalid MCP HTTP header annotation; call was not sent",
            ));
        }
        let call = connection.call_tool(
            &registered.original_name,
            input,
            &registered.definition.input_schema,
        );
        let result = tokio::select! {
            biased;
            _ = context.cancellation.cancelled() => bail!("Cancelled during MCP call; outcome unknown"),
            result = tokio::time::timeout(Duration::from_secs(registered.server.config.call_timeout_secs), call) => result,
        };
        match result {
            Err(_) => Ok(ToolResult::error(
                "MCP call timed out; outcome unknown. Do not replay this call.",
            )),
            Ok(Err(error)) => Ok(ToolResult::error(connection.redact(&error.to_string()))),
            Ok(Ok(result)) => Ok(convert_result(result)),
        }
    }
}

/// Stable ASCII tool names, with a deterministic suffix for truncated names.
pub fn tool_name(server: &str, tool: &str) -> String {
    let original = format!("mcp__{server}__{tool}");
    let mut name: String = original
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.len() > 64 {
        let hash = format!("{:x}", Sha256::digest(original.as_bytes()));
        name.truncate(55);
        name.push('_');
        name.push_str(&hash[..8]);
    }
    name
}

fn convert_result(result: Value) -> ToolResult {
    if let Some(kind) = result.get("resultType")
        && kind != "complete"
    {
        return ToolResult::error(
            "Unsupported MCP result type or input request; outcome unknown. Do not replay this call.",
        );
    }
    let Some(content) = result.get("content").and_then(Value::as_array) else {
        return ToolResult::error("Invalid MCP tool result: missing content array");
    };
    let mut blocks = Vec::new();
    let mut has_text = false;
    for block in content {
        if block["type"] == "text" {
            if let Some(text) = block["text"].as_str() {
                has_text = true;
                blocks.push(text.to_owned());
            } else {
                return ToolResult::error("Invalid MCP text content");
            }
        } else {
            let kind = match block["type"].as_str() {
                Some("image") => "image",
                Some("audio") => "audio",
                Some("resource") => "resource",
                Some("resource_link") => "resource link",
                _ => "unknown content",
            };
            let bytes = block["data"]
                .as_str()
                .map(decoded_size)
                .unwrap_or_else(|| block["resource"].to_string().len());
            blocks.push(format!("[MCP {kind}: {bytes} bytes omitted]"));
        }
    }
    if !has_text && let Some(structured) = result.get("structuredContent") {
        blocks.insert(0, structured.to_string());
    }
    ToolResult {
        content: blocks.join("\n"),
        is_error: result["isError"].as_bool().unwrap_or(false),
        pending: false,
    }
}
fn decoded_size(data: &str) -> usize {
    let padding = data
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'=')
        .count()
        .min(2);
    (data.len() * 3 / 4).saturating_sub(padding)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bubblepaw_core::Approval;

    #[test]
    fn names_are_bounded_ascii_and_stable() {
        assert_eq!(tool_name("a.b", "read/issue"), "mcp__a_b__read_issue");
        let long = "x".repeat(120);
        let name = tool_name("github", &long);
        assert_eq!(name.len(), 64);
        assert_eq!(name, tool_name("github", &long));
        assert_ne!(name, tool_name("github", &(long + "y")));
        assert_eq!(tool_name("é", "🐾"), "mcp______");
    }
    #[test]
    fn defaults_require_approval_and_debug_omits_values() {
        let mut config: McpServerSettings =
            serde_json::from_value(json!({"command":"test"})).unwrap();
        assert_eq!(config.approval, Approval::Confirm);
        assert_eq!(config.startup_timeout_secs, 30);
        assert_eq!(config.call_timeout_secs, 120);
        config
            .env
            .insert("NON_SECRET".into(), "should-not-print".into());
        config.env_from.insert("CHILD".into(), "HOST_NAME".into());
        let debug = format!("{config:?}");
        assert!(!debug.contains("should-not-print"));
        assert!(debug.contains("HOST_NAME"));
    }
    #[test]
    fn results_preserve_text_and_bound_binary_content() {
        let result = convert_result(
            json!({"content":[{"type":"text","text":"a"},{"type":"image","data":"YWJjZA=="},{"type":"text","text":"b"}],"structuredContent":{"ignored":true},"isError":true}),
        );
        assert_eq!(result.content, "a\n[MCP image: 4 bytes omitted]\nb");
        assert!(result.is_error);
        let result = convert_result(json!({"content":[],"structuredContent":{"x":2}}));
        assert_eq!(result.content, "{\"x\":2}");
        assert!(!result.is_error);
    }
    #[test]
    fn input_requests_and_unknown_result_types_are_not_completed_or_replayed() {
        for kind in ["input_required", "unrecognized"] {
            let result = convert_result(json!({"resultType":kind,"content":[],"inputRequests":[]}));
            assert!(result.is_error);
            assert!(result.content.contains("outcome unknown"));
            assert!(result.content.contains("Do not replay"));
        }
        let result = convert_result(
            json!({"resultType":"complete","content":[{"type":"text","text":"done"}]}),
        );
        assert_eq!(result.content, "done");
        assert!(!result.is_error);
    }
}
