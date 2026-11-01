use crate::{McpClientEnv, McpServerSettings, McpSettings, transport::Connection};
use anyhow::{Result, anyhow, bail, ensure};
use bubblepaw_core::{CancellationToken, Tool, ToolDefinition, ToolRegistry};
use serde::Serialize;
use serde_json::{Value, json};
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

mod auth;
mod follow;
mod inventory;
mod registry;
mod state;
mod status;
mod tool;
use inventory::{definitions, inventory_diff, mark_removed, rebuild};
use state::{Entry, Registered};
pub use status::{McpAuthState, McpAuthStatus, McpServerState, McpServerStatus, McpToolStatus};
use tool::McpTool;
pub use tool::tool_name;

/// Host-scoped startup options beyond the settings.
#[derive(Default)]
pub struct McpHostOptions {
    /// Request-scoped client environment; see `start_with_client_env`.
    pub client_env: Option<McpClientEnv>,
    /// Folder for OAuth tokens (`<data dir>/auth`). OAuth servers fail to start
    /// without it.
    pub auth_dir: Option<PathBuf>,
}

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
    fn absorb(&mut self, other: McpDiff) {
        self.added.extend(other.added);
        self.removed.extend(other.removed);
        self.changed.extend(other.changed);
        self.started.extend(other.started);
        self.stopped.extend(other.stopped);
        self.restarted.extend(other.restarted);
        self.filtered.extend(other.filtered);
        self.warnings.extend(other.warnings);
    }
}

/// Shared host inventory. Calls pin generations; retained tool handles do not.
pub struct McpHost {
    inner: Arc<HostInner>,
    /// Only the owning handle stops servers on drop; internal handles that
    /// restart a server after sign-in do not.
    owner: bool,
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
    /// The MCP section this host follows, once subscribed.
    section: Mutex<Option<tokio::sync::watch::Receiver<Arc<McpSettings>>>>,
    auth_dir: Option<PathBuf>,
    /// Sign-in state per OAuth server, kept across that server's restarts.
    auth: Mutex<BTreeMap<String, Arc<crate::oauth::ServerAuth>>>,
    /// Names (never values) of the stored header secrets per HTTP server.
    header_names: Mutex<BTreeMap<String, Vec<String>>>,
    status: tokio::sync::broadcast::Sender<McpServerStatus>,
    /// The last status sent per server, so only changes are sent.
    sent: Mutex<BTreeMap<String, McpServerStatus>>,
    /// Waiting sign-ins; aborted with the host.
    background: Mutex<JoinSet<()>>,
}
#[derive(Default)]
struct HostState {
    servers: BTreeMap<String, Entry>,
    tools: BTreeMap<String, Registered>,
    removed: BTreeMap<String, String>,
    warnings: Vec<String>,
    /// The last applied settings, including disabled servers.
    settings: McpSettings,
    /// Why an enabled server is not serving.
    failures: BTreeMap<String, String>,
    /// Changes from sign-in restarts, reported by the next `refresh_tools`.
    pending: McpDiff,
}
impl McpHost {
    /// Standalone startup: credentials and child env come from this process.
    pub async fn start(settings: &McpSettings, workspace: &Path) -> Self {
        Self::start_with_options(settings, workspace, McpHostOptions::default()).await
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
        let options = McpHostOptions {
            client_env: Some(client_env),
            auth_dir: None,
        };
        Self::start_with_options(settings, workspace, options).await
    }

    /// Startup with a client snapshot and an OAuth token folder.
    pub async fn start_with_options(
        settings: &McpSettings,
        workspace: &Path,
        options: McpHostOptions,
    ) -> Self {
        let host = Self {
            inner: Arc::new(HostInner {
                workspace: workspace.to_owned(),
                client_env: options.client_env.map(Arc::new),
                state: Mutex::default(),
                gate: tokio::sync::Mutex::new(()),
                reapers: Mutex::new(JoinSet::new()),
                retired: Mutex::default(),
                closed: AtomicBool::new(false),
                section: Mutex::new(None),
                auth_dir: options.auth_dir,
                auth: Mutex::default(),
                header_names: Mutex::default(),
                status: tokio::sync::broadcast::channel(64).0,
                sent: Mutex::default(),
                background: Mutex::new(JoinSet::new()),
            }),
            owner: true,
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
        self.inner.drop_stale_tokens(settings);
        let (mut jobs, mut filter_updates) = (JoinSet::new(), BTreeMap::new());
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
                let auth = self.inner.auth_for(&name, config);
                let server = Arc::new(Server::new(
                    config.clone(),
                    settings.header_key_envs.clone(),
                    self.inner.workspace.clone(),
                    self.inner.client_env.clone(),
                    auth.as_ref().ok().cloned().flatten(),
                    self.inner.secrets_path(&name),
                ));
                jobs.spawn(async move {
                    if let Err(error) = auth {
                        return (name, server, Err(error.to_string()));
                    }
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
        let previous;
        {
            let mut state = self.inner.state.lock().unwrap();
            let before = definitions(&state);
            let removed: Vec<_> = state
                .servers
                .keys()
                .filter(|name| !settings.servers.get(*name).is_some_and(|c| c.enabled))
                .cloned()
                .collect();
            state
                .failures
                .retain(|name, _| settings.servers.get(name).is_some_and(|c| c.enabled));
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
                        state.failures.remove(&name);
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
                        state
                            .failures
                            .insert(name.clone(), format!("{error}{suffix}"));
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
            previous = std::mem::replace(&mut state.settings, settings.clone());
        }
        let gone: Vec<String> = previous
            .servers
            .keys()
            .filter(|name| !settings.servers.contains_key(*name))
            .cloned()
            .collect();
        for name in &gone {
            self.inner.forget_secrets(name);
        }
        self.inner.load_header_names(settings);
        self.inner.auth.lock().unwrap().retain(|name, auth| {
            let keep = settings
                .servers
                .get(name)
                .is_some_and(|c| c.oauth && c.url.is_some());
            if !keep {
                auth.close();
            }
            keep
        });
        for server in retired {
            self.retire(server);
        }
        self.reap_finished();
        self.inner.publish();
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
        let pending = std::mem::take(&mut self.inner.state.lock().unwrap().pending);
        diff.absorb(pending);
        self.inner.publish();
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
        self.inner.background.lock().unwrap().abort_all();
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
        if !self.owner {
            return;
        }
        self.inner.background.lock().unwrap().abort_all();
        for auth in self.inner.auth.lock().unwrap().values() {
            auth.close();
        }
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
    auth: Option<Arc<crate::oauth::ServerAuth>>,
    /// The server's secret file, read on each connection for stored headers.
    secrets_path: Option<PathBuf>,
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
        auth: Option<Arc<crate::oauth::ServerAuth>>,
        secrets_path: Option<PathBuf>,
    ) -> Self {
        Self {
            config,
            allowed_header_envs,
            workspace,
            client_env,
            auth,
            secrets_path,
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
        let (config, workspace, envs, client_env, auth, secrets) = (
            self.config.clone(),
            self.workspace.clone(),
            self.allowed_header_envs.clone(),
            self.client_env.clone(),
            self.auth.clone(),
            self.secrets_path.clone(),
        );
        let connection = tokio::task::spawn_blocking(move || {
            let stored = secrets
                .filter(|_| config.url.is_some())
                .map(|path| crate::oauth::store::read_headers(&path))
                .unwrap_or_default();
            Connection::open(
                &config,
                &workspace,
                &envs,
                client_env.as_deref(),
                auth,
                &stored,
            )
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
        match self.discover().await {
            // A rejected access token is refreshed and discovery, which is
            // read-only, runs once more.
            Err(_) if self.auth.as_ref().is_some_and(|auth| auth.take_retry()) => {
                let failed = self.connection.lock().unwrap().take();
                if let Some(connection) = failed {
                    connection.shutdown(Duration::ZERO).await;
                }
                self.discover().await
            }
            result => result,
        }
    }
    async fn discover(&self) -> Result<Vec<Value>> {
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
