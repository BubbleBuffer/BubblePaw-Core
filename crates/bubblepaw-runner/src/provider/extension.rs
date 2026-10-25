//! Out-of-process provider extensions (docs/providers.md): line-delimited
//! JSON-RPC 2.0 over the child's stdin/stdout. The child is spawned on first
//! use and respawned after it exits; a respawned extension receives its next
//! step for each session with a fresh cursor so a stateful mirror rebuilds.
use super::stream::TextBatch;
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Capabilities, CursorResets, ModelInfo, ModelRequest, ModelResponse,
    Provider, ProviderError, ProviderErrorKind, TextCallback, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

/// One entry of the extensions file: `[{"id","command","args","cwd"?,"env"?}]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtensionConfig {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Variables set on this instance's process on top of the inherited
    /// environment, e.g. a per-account `CLAUDE_CONFIG_DIR`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// Names the variables an instance's `env` sets explicitly (comma-separated), so
/// an extension can tell them from inherited ones.
pub const EXTENSION_ENV_KEYS: &str = "BUBBLEPAW_EXTENSION_ENV_KEYS";

/// Whether `name` can be set on a child process and listed in [`EXTENSION_ENV_KEYS`].
pub fn valid_env_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['=', ',', '\0'])
}

/// Load an extensions file. A relative `cwd` is resolved against the file's directory.
pub fn load_extensions(path: impl AsRef<Path>) -> Result<Vec<ExtensionConfig>> {
    let path = path.as_ref();
    let bytes = std::fs::read(path)
        .with_context(|| format!("cannot read extensions file {}", path.display()))?;
    let mut entries: Vec<ExtensionConfig> = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid extensions file {}", path.display()))?;
    let base = path.parent().unwrap_or(Path::new("."));
    for entry in &mut entries {
        anyhow::ensure!(
            !entry.id.is_empty() && !entry.command.is_empty(),
            "extension entries need an id and a command"
        );
        anyhow::ensure!(
            entry.env.keys().all(|name| valid_env_name(name)),
            "extension env names must be nonempty and contain no '=', ',' or NUL"
        );
        if let Some(cwd) = &entry.cwd
            && cwd.is_relative()
        {
            entry.cwd = Some(base.join(cwd));
        }
    }
    Ok(entries)
}

#[derive(Debug, Clone, Deserialize)]
struct InitializedModel {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    capabilities: Capabilities,
}
#[derive(Debug, Clone, Deserialize)]
struct Initialized {
    #[serde(default)]
    models: Vec<InitializedModel>,
    #[serde(default)]
    capabilities: ExtensionCapabilities,
}
/// Request options supported by the extension protocol implementation, separate
/// from each model's core capabilities.
#[derive(Debug, Clone, Default, Deserialize)]
struct ExtensionCapabilities {
    #[serde(default)]
    reasoning_effort: bool,
}

/// A settled request: its outcome and the model the SDK reported for it. The model
/// names the usage row, so it travels with an error too.
struct Settled {
    outcome: std::result::Result<Value, ProviderError>,
    reported_model: Option<String>,
}
struct Pending {
    reply: oneshot::Sender<Settled>,
    on_text: Option<TextCallback>,
}
struct Shared {
    id: String,
    /// Credential values from the instance's `env`, redacted from diagnostics.
    secrets: Vec<String>,
    pending: Mutex<HashMap<u64, Pending>>,
    alive: AtomicBool,
    next_id: AtomicU64,
}
impl Shared {
    fn fail_all(&self, reason: &str) {
        self.alive.store(false, Ordering::SeqCst);
        let drained: Vec<Pending> = self
            .pending
            .lock()
            .unwrap()
            .drain()
            .map(|(_, pending)| pending)
            .collect();
        let message = format!("extension {} {reason}", self.id);
        for pending in drained {
            let _ = pending.reply.send(Settled {
                outcome: Err(ProviderError::new(
                    ProviderErrorKind::Transient,
                    message.clone(),
                )),
                reported_model: None,
            });
        }
    }
    fn dispatch(&self, line: &str) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            tracing::warn!(target: "bubblepaw::extension", extension = %self.id, "ignoring invalid JSON-RPC line");
            return;
        };
        if let Some(method) = value.get("method").and_then(Value::as_str) {
            if method == "text"
                && let Some(id) = value["params"]["id"].as_u64()
                && let Some(text) = value["params"]["text"].as_str()
            {
                let callback = self
                    .pending
                    .lock()
                    .unwrap()
                    .get(&id)
                    .and_then(|pending| pending.on_text.clone());
                if let Some(callback) = callback {
                    callback(text);
                }
            }
            return;
        }
        let Some(id) = value.get("id").and_then(Value::as_u64) else {
            return;
        };
        let Some(pending) = self.pending.lock().unwrap().remove(&id) else {
            return;
        };
        let settled = match value.get("error") {
            Some(error) => Settled {
                outcome: Err(rpc_error(error, &self.secrets)),
                reported_model: reported_model_of(error.get("data")),
            },
            None => {
                let result = value.get("result").cloned().unwrap_or(Value::Null);
                Settled {
                    reported_model: reported_model_of(Some(&result)),
                    outcome: Ok(result),
                }
            }
        };
        let _ = pending.reply.send(settled);
    }
}

struct Connection {
    shared: Arc<Shared>,
    outgoing: mpsc::UnboundedSender<String>,
    generation: u64,
    _child: Child,
    /// Dropping the connection kills the host and anything it started.
    tree: bubblepaw_process::ProcessTree,
    tasks: Vec<JoinHandle<()>>,
}
impl Drop for Connection {
    fn drop(&mut self) {
        // Kill the host's whole tree, then release the reader, writer and stderr
        // loops instead of leaving them detached on a closed connection.
        self.tree.kill();
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}
impl Connection {
    fn spawn(
        config: &ExtensionConfig,
        inherited: Option<&[(String, String)]>,
        generation: u64,
    ) -> Result<Self> {
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // A client snapshot replaces the host environment entirely, so the
        // extension sees what the invoking client saw and nothing else.
        if let Some(inherited) = inherited {
            command.env_clear().envs(inherited.iter().cloned());
        }
        command.envs(&config.env).env(
            EXTENSION_ENV_KEYS,
            config.env.keys().cloned().collect::<Vec<_>>().join(","),
        );
        if let Some(cwd) = &config.cwd {
            command.current_dir(cwd);
        }
        let (mut child, tree) = bubblepaw_process::spawn(&mut command)
            .with_context(|| format!("cannot start extension {}", config.id))?;
        let stdin = child.stdin.take().context("extension stdin")?;
        let stdout = child.stdout.take().context("extension stdout")?;
        let stderr = child.stderr.take().context("extension stderr")?;
        let shared = Arc::new(Shared {
            id: config.id.clone(),
            secrets: env_secrets(&config.env),
            pending: Mutex::default(),
            alive: AtomicBool::new(true),
            next_id: AtomicU64::new(1),
        });
        let (outgoing, mut lines) = mpsc::unbounded_channel::<String>();
        let writer = shared.clone();
        let writer_task = tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(line) = lines.recv().await {
                let written = async {
                    stdin.write_all(line.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await
                }
                .await;
                if written.is_err() {
                    writer.fail_all("closed its input");
                    return;
                }
            }
        });
        let reader = shared.clone();
        let reader_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                reader.dispatch(&line);
            }
            reader.fail_all("exited");
        });
        let errors = shared.clone();
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = diagnostic_line(&line, &errors.secrets);
                if !line.is_empty() {
                    tracing::warn!(target: "bubblepaw::extension", extension = %errors.id, "extension stderr: {line}");
                }
            }
        });
        Ok(Self {
            shared,
            outgoing,
            generation,
            _child: child,
            tree,
            tasks: vec![writer_task, reader_task, stderr_task],
        })
    }
    fn alive(&self) -> bool {
        self.shared.alive.load(Ordering::SeqCst)
    }
    fn request(
        &self,
        method: &str,
        params: Value,
        on_text: Option<TextCallback>,
    ) -> Result<(u64, oneshot::Receiver<Settled>)> {
        let id = self.shared.next_id.fetch_add(1, Ordering::SeqCst);
        let (reply, receiver) = oneshot::channel();
        self.shared
            .pending
            .lock()
            .unwrap()
            .insert(id, Pending { reply, on_text });
        // Insert first, then check: an exit either drains this entry or is seen here.
        let line = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string();
        if !self.alive() || self.outgoing.send(line).is_err() {
            self.forget(id);
            return Err(ProviderError::new(
                ProviderErrorKind::Transient,
                format!("extension {} is not running", self.shared.id),
            )
            .into());
        }
        Ok((id, receiver))
    }
    fn notify(&self, method: &str, params: Value) {
        let _ = self
            .outgoing
            .send(json!({"jsonrpc":"2.0","method":method,"params":params}).to_string());
    }
    fn forget(&self, id: u64) {
        self.shared.pending.lock().unwrap().remove(&id);
    }
}

/// Sends `cancel` if the step is abandoned (for example when the harness drops
/// the provider future on cancellation) before its reply arrived.
struct StepGuard {
    connection: Arc<Connection>,
    id: u64,
    armed: bool,
    cancel_sent: bool,
}
impl StepGuard {
    fn cancel(&mut self) {
        if !self.cancel_sent {
            self.cancel_sent = true;
            self.connection.notify("cancel", json!({"id":self.id}));
        }
    }
}
impl Drop for StepGuard {
    fn drop(&mut self) {
        if self.armed {
            self.connection.forget(self.id);
            self.cancel();
        }
    }
}

pub struct ExtensionProvider {
    config: ExtensionConfig,
    /// Client environment the child starts from; `None` inherits the host's.
    inherited: Option<Vec<(String, String)>>,
    connection: tokio::sync::Mutex<Option<Arc<Connection>>>,
    info: Mutex<Option<Initialized>>,
    generation: AtomicU64,
    seen: Mutex<HashMap<String, u64>>,
    resets: CursorResets,
    initialize_timeout: Duration,
    cancel_grace: Duration,
}
impl ExtensionProvider {
    pub fn new(config: ExtensionConfig) -> Self {
        Self {
            config,
            inherited: None,
            connection: tokio::sync::Mutex::new(None),
            info: Mutex::new(None),
            generation: AtomicU64::new(0),
            seen: Mutex::default(),
            resets: CursorResets::new(),
            initialize_timeout: Duration::from_secs(30),
            cancel_grace: Duration::from_secs(5),
        }
    }
    /// Start the child from this environment snapshot instead of the host's.
    pub fn with_inherited_env(mut self, env: Vec<(String, String)>) -> Self {
        self.inherited = Some(env);
        self
    }
    pub fn id(&self) -> &str {
        &self.config.id
    }
    /// Spawn and initialize now instead of on first use.
    pub async fn start(&self) -> Result<()> {
        self.connect().await.map(|_| ())
    }
    async fn connect(&self) -> Result<Arc<Connection>> {
        let mut slot = self.connection.lock().await;
        if let Some(connection) = slot.as_ref().filter(|c| c.alive()) {
            return Ok(connection.clone());
        }
        *slot = None;
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let spawned = Connection::spawn(&self.config, self.inherited.as_deref(), generation);
        let connection = Arc::new(spawned.map_err(|error| {
            anyhow::Error::new(ProviderError::new(
                ProviderErrorKind::Fatal,
                format!("{error:#}"),
            ))
        })?);
        let (_, receiver) = connection.request(
            "initialize",
            json!({"host":"bubblepaw","version":env!("CARGO_PKG_VERSION")}),
            None,
        )?;
        let settled = tokio::time::timeout(self.initialize_timeout, receiver)
            .await
            .map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::Transient,
                    format!("extension {} did not initialize in time", self.config.id),
                )
            })?
            .map_err(|_| {
                anyhow::Error::new(ProviderError::new(
                    ProviderErrorKind::Transient,
                    format!("extension {} exited", self.config.id),
                ))
            })?;
        let reply = settled.outcome?;
        let initialized: Initialized = serde_json::from_value(reply).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Fatal,
                format!(
                    "extension {} sent an invalid initialize result",
                    self.config.id
                ),
            )
        })?;
        *self.info.lock().unwrap() = Some(initialized);
        *slot = Some(connection.clone());
        Ok(connection)
    }
}
#[async_trait]
impl Provider for ExtensionProvider {
    /// Known after the extension has been started (first step or `start`).
    fn models(&self) -> Vec<ModelInfo> {
        self.info
            .lock()
            .unwrap()
            .as_ref()
            .map(|info| {
                info.models
                    .iter()
                    .map(|model| ModelInfo {
                        id: model.id.clone(),
                        name: model.name.clone().unwrap_or_else(|| model.id.clone()),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
    /// Defaults until the extension has been started; `generate` reapplies the
    /// tool surface itself after initialization, so the first step is correct.
    fn capabilities(&self, model: &str) -> Capabilities {
        self.info
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|info| info.models.iter().find(|m| m.id == model))
            .map(|model| model.capabilities.clone())
            .unwrap_or_default()
    }
    async fn generate(
        &self,
        mut request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        if cancellation.is_cancelled() {
            return Err(ProviderError::new(ProviderErrorKind::Transient, "cancelled").into());
        }
        let connection = self.connect().await?;
        {
            let info = self.info.lock().unwrap();
            let capabilities = &info
                .as_ref()
                .expect("connected extension initialized")
                .capabilities;
            if request.reasoning_effort.is_some() && !capabilities.reasoning_effort {
                return Err(ProviderError::new(
                    ProviderErrorKind::Fatal,
                    format!(
                        "extension {} does not support reasoning_effort",
                        self.config.id
                    ),
                )
                .into());
            }
        }
        if !request.session_id.is_empty() {
            let previous = self
                .seen
                .lock()
                .unwrap()
                .insert(request.session_id.clone(), connection.generation);
            if connection.generation > 1 && previous != Some(connection.generation) {
                self.resets.reset(&request.session_id, &mut request.history);
            } else {
                self.resets.apply(&request.session_id, &mut request.history);
            }
        }
        // Harness selects the tool surface, including an explicit native mode.
        // Forward its declarations unchanged so introspection matches the wire.
        let mut params = serde_json::to_value(&request)?;
        // Legacy callers may still set this field. Structured results use the
        // runtime submit_result tool; response schemas never cross the RPC wire.
        params.as_object_mut().unwrap().remove("output_schema");
        if cancellation.is_cancelled() {
            return Err(ProviderError::new(ProviderErrorKind::Transient, "cancelled").into());
        }
        let (text_tx, mut text_rx) = mpsc::unbounded_channel::<String>();
        let queue_text: TextCallback = Arc::new(move |text| {
            let _ = text_tx.send(text.to_owned());
        });
        let mut meter = super::PhysicalRequest::new(&self.config.id, &request.model);
        let (id, mut receiver) = connection.request("step", params, Some(queue_text))?;
        let (on_text, published) = super::track_text_publication(on_text);
        let mut batch = TextBatch::new(on_text);
        let mut guard = StepGuard {
            connection: connection.clone(),
            id,
            armed: true,
            cancel_sent: false,
        };
        let mut text_open = true;
        let (reply, cancelled) = loop {
            let due = batch.due();
            tokio::select! {
                reply = &mut receiver => break (reply, false),
                text = text_rx.recv(), if text_open => {
                    match text {
                        Some(text) => batch.push(&text),
                        None => text_open = false,
                    }
                }
                _ = async {
                    if let Some(due) = due {
                        tokio::time::sleep_until(due).await;
                    }
                }, if due.is_some() => batch.flush(),
                _ = cancellation.cancelled() => {
                    guard.cancel();
                    let reply = match tokio::time::timeout(self.cancel_grace, &mut receiver).await {
                        Ok(reply) => reply,
                        Err(_) => Ok(Settled {
                            outcome: Err(ProviderError::new(ProviderErrorKind::Transient, "cancelled")),
                            reported_model: None,
                        }),
                    };
                    break (reply, true);
                }
            }
        };
        guard.armed = false;
        connection.forget(id);
        if !cancelled {
            // The stdout reader enqueues notifications before the terminal reply.
            // Drain them even when select observes the reply before the text.
            while let Ok(text) = text_rx.try_recv() {
                batch.push(&text);
            }
            batch.flush();
        }
        let settled = reply.unwrap_or_else(|_| Settled {
            outcome: Err(ProviderError::new(
                ProviderErrorKind::Transient,
                format!("extension {} exited", self.config.id),
            )),
            reported_model: None,
        });
        if let Some(model) = &settled.reported_model {
            meter.name_model(model);
        }
        let result = (|| -> Result<ModelResponse> {
            let value = settled.outcome.map_err(anyhow::Error::new)?;
            let response = decode_step_result(
                value,
                &self.config.id,
                self.config.account.as_deref().unwrap_or(&self.config.id),
            )?;
            validate_calls(response, &request, &self.config.id)
        })();
        meter.finish_result(&result);
        result.map_err(|error| super::mark_published(error, published.load(Ordering::Acquire)))
    }
    async fn close_session(&self, session_id: &str) -> Result<()> {
        self.resets.forget(session_id);
        self.seen.lock().unwrap().remove(session_id);
        let connection = self.connection.lock().await.clone();
        let Some(connection) = connection.filter(|c| c.alive()) else {
            return Ok(());
        };
        let (_, receiver) =
            connection.request("session.close", json!({"session_id":session_id}), None)?;
        match tokio::time::timeout(self.initialize_timeout, receiver).await {
            Ok(Ok(settled)) => settled.outcome.map(|_| ()).map_err(anyhow::Error::from),
            Ok(Err(_)) => Ok(()),
            Err(_) => Err(ProviderError::new(
                ProviderErrorKind::Transient,
                format!(
                    "extension {} did not close the session in time",
                    self.config.id
                ),
            )
            .into()),
        }
    }
}

#[cfg(test)]
mod quota_tests;

mod errors;
use errors::{diagnostic_line, env_secrets, extension_quota, reported_model_of, rpc_error};
mod result;
use result::{decode_step_result, validate_calls};
