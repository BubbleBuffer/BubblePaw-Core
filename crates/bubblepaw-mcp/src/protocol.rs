use crate::{McpClientEnv, McpServerSettings};
use anyhow::{Result, anyhow, bail};
use bubblepaw_core::CancellationToken;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    ffi::OsString,
    path::Path,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader},
    process::Command,
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

const FRAME_LIMIT: usize = 8 * 1024 * 1024;
const STDERR_LIMIT: usize = 4096;
type Reply = std::result::Result<Value, String>;

/// Kept only with the live transport, never exposed by Debug or serialized.
#[derive(Default)]
struct Redactor(Vec<String>);
impl Redactor {
    fn text(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for secret in &self.0 {
            text = text.replace(secret, "[redacted]");
        }
        text
    }
    fn value(&self, value: &mut Value) {
        match value {
            Value::String(text) => *text = self.text(text),
            Value::Array(values) => {
                for value in values {
                    self.value(value);
                }
            }
            Value::Object(values) => {
                let original = std::mem::take(values);
                for (key, mut value) in original {
                    self.value(&mut value);
                    values.insert(self.text(&key), value);
                }
            }
            _ => {}
        }
    }
    /// Keep enough uncommitted bytes to match a secret spanning read chunks.
    /// Redact before ring truncation, so even a cut through a very long secret
    /// cannot expose a suffix in startup diagnostics.
    fn prefix(&self, pending: &mut Vec<u8>, final_chunk: bool) -> Vec<u8> {
        let carry = self
            .0
            .iter()
            .map(String::len)
            .max()
            .unwrap_or(0)
            .saturating_sub(1);
        let boundary = if final_chunk {
            pending.len()
        } else {
            pending.len().saturating_sub(carry)
        };
        let mut output = Vec::new();
        let mut offset = 0;
        while offset < boundary {
            if let Some(secret) = self
                .0
                .iter()
                .find(|secret| pending[offset..].starts_with(secret.as_bytes()))
            {
                output.extend_from_slice(b"[redacted]");
                offset += secret.len();
            } else {
                output.push(pending[offset]);
                offset += 1;
            }
        }
        pending.drain(..offset);
        output
    }
}

struct Shared {
    pending: Mutex<BTreeMap<u64, oneshot::Sender<Reply>>>,
    alive: AtomicBool,
    tools_changed: AtomicBool,
    next_id: AtomicU64,
    stderr: Mutex<VecDeque<u8>>,
    redactor: Redactor,
    failed: CancellationToken,
    done: CancellationToken,
    close_input: CancellationToken,
}
impl Shared {
    fn fail_all(&self, message: &str) {
        self.alive.store(false, Ordering::SeqCst);
        let pending = std::mem::take(&mut *self.pending.lock().unwrap());
        for (_, reply) in pending {
            let _ = reply.send(Err(message.into()));
        }
    }
    fn dispatch(&self, line: &[u8]) -> Result<()> {
        let mut message: Value =
            serde_json::from_slice(line).map_err(|_| anyhow!("invalid MCP JSON response"))?;
        if message["jsonrpc"] != "2.0" {
            bail!("invalid MCP JSON-RPC version");
        }
        self.redactor.value(&mut message);
        if message["method"].as_str().is_some() {
            if message["method"] == "notifications/tools/list_changed"
                && message.get("id").is_none()
            {
                self.tools_changed.store(true, Ordering::SeqCst);
            }
            // Unsupported server requests receive a method-not-found response
            // in the reader. Notifications need no response.
            return Ok(());
        }
        let Some(id) = message["id"].as_u64() else {
            bail!("invalid MCP response id");
        };
        let reply = if let Some(error) = message.get("error") {
            let text = error["message"].as_str().unwrap_or("server error");
            let code = error["code"].as_i64().unwrap_or(-32000);
            Err(format!("MCP JSON-RPC error {code}: {text}"))
        } else if let Some(result) = message.get("result") {
            Ok(result.clone())
        } else {
            bail!("invalid MCP response envelope");
        };
        if let Some(sender) = self.pending.lock().unwrap().remove(&id) {
            let _ = sender.send(reply);
        }
        Ok(())
    }
    fn stderr(&self) -> String {
        let bytes: Vec<u8> = self.stderr.lock().unwrap().iter().copied().collect();
        self.redactor.text(&String::from_utf8_lossy(&bytes))
    }
}

pub(crate) struct Connection {
    shared: Arc<Shared>,
    outgoing: Mutex<Option<mpsc::UnboundedSender<String>>>,
    stop: Mutex<Option<oneshot::Sender<Duration>>>,
    supervisor: Mutex<Option<JoinHandle<()>>>,
    /// Dropping the connection kills the server's whole tree, on both OSes.
    tree: bubblepaw_process::ProcessTree,
}
impl Connection {
    /// Child env: host env (inherited), then config `env`, then config
    /// `env_from`. With a client snapshot, `env_from` resolves only from it and
    /// no other snapshot entry reaches this child, so each server receives only
    /// the values its own `env_from` names. Snapshot values are secrets.
    pub(crate) fn spawn(
        config: &McpServerSettings,
        workspace: &Path,
        client_env: Option<&McpClientEnv>,
    ) -> Result<Arc<Self>> {
        let mut command = Command::new(&config.command);
        let mut secrets = Vec::new();
        command
            .args(&config.args)
            .envs(&config.env)
            .current_dir(
                config
                    .cwd
                    .as_ref()
                    .map(|cwd| workspace.join(cwd))
                    .unwrap_or_else(|| workspace.to_owned()),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (child_name, host_name) in &config.env_from {
            // With a client snapshot, env_from never falls back to host env.
            // The missing-variable text is the standalone text, on purpose.
            let missing = || anyhow!("required host environment variable is missing");
            let value = match client_env {
                Some(client_env) => client_env
                    .get(host_name)
                    .map(OsString::from)
                    .ok_or_else(missing)?,
                None => std::env::var_os(host_name).ok_or_else(missing)?,
            };
            if let Some(value) = value.to_str().filter(|value| !value.is_empty()) {
                secrets.push(value.to_owned());
            }
            command.env(child_name, value);
        }
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        secrets.dedup();
        // OS errors can contain executable/cwd data; avoid printing command
        // contents or environment values in diagnostics.
        let (mut child, tree) = bubblepaw_process::spawn(&mut command)
            .map_err(|error| anyhow!("cannot spawn MCP server ({:?})", error.kind()))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("MCP stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("MCP stdout unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("MCP stderr unavailable"))?;
        let shared = Arc::new(Shared {
            pending: Mutex::default(),
            alive: AtomicBool::new(true),
            tools_changed: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            stderr: Mutex::default(),
            redactor: Redactor(secrets),
            failed: CancellationToken::new(),
            done: CancellationToken::new(),
            close_input: CancellationToken::new(),
        });
        let (outgoing, mut messages) = mpsc::unbounded_channel::<String>();
        let writer_shared = shared.clone();
        let writer = tokio::spawn(async move {
            loop {
                let message = tokio::select! {
                    biased;
                    _ = writer_shared.close_input.cancelled() => break,
                    message = messages.recv() => match message { Some(message) => message, None => break },
                };
                let written = async {
                    stdin.write_all(message.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await
                }
                .await;
                if written.is_err() {
                    writer_shared.fail_all("server exited; outcome unknown");
                    writer_shared.failed.cancel();
                    break;
                }
            }
            let _ = stdin.shutdown().await;
        });
        let reader_shared = shared.clone();
        let reader_outgoing = outgoing.clone();
        let reader = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            while let Ok(Some(line)) = read_frame(&mut reader, FRAME_LIMIT).await {
                // Sampling, roots and other server-initiated requests are not
                // supported. Reply so the server does not wait indefinitely.
                if let Ok(message) = serde_json::from_slice::<Value>(&line)
                    && message.get("method").is_some()
                    && let Some(id) = message.get("id")
                {
                    let _ = reader_outgoing.send(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not supported"}}).to_string());
                }
                if reader_shared.dispatch(&line).is_err() {
                    break;
                }
            }
            reader_shared.fail_all("server exited; outcome unknown");
            reader_shared.failed.cancel();
        });
        let stderr_shared = shared.clone();
        let stderr_task = tokio::spawn(async move { drain_stderr(stderr, stderr_shared).await });
        let (stop, stopping) = oneshot::channel();
        let owned = shared.clone();
        let supervisor_tree = tree.clone();
        let supervisor = tokio::spawn(async move {
            // Capture the guard before spawning so even an unpolled/aborted
            // supervisor still terminates the owned process tree.
            let tree = supervisor_tree;
            tokio::select! {
                _ = child.wait() => {},
                _ = owned.failed.cancelled() => {},
                grace = stopping => {
                    let grace = grace.unwrap_or(Duration::ZERO);
                    let _ = tokio::time::timeout(grace, child.wait()).await;
                }
            }
            owned.fail_all("server exited; outcome unknown");
            tree.kill();
            let _ = child.kill().await;
            let _ = child.wait().await;
            for mut task in [writer, reader, stderr_task] {
                if tokio::time::timeout(Duration::from_millis(100), &mut task)
                    .await
                    .is_err()
                {
                    task.abort();
                    let _ = task.await;
                }
            }
            owned.done.cancel();
        });
        Ok(Arc::new(Self {
            shared,
            outgoing: Mutex::new(Some(outgoing)),
            stop: Mutex::new(Some(stop)),
            supervisor: Mutex::new(Some(supervisor)),
            tree,
        }))
    }

    pub(crate) fn take_tools_changed(&self) -> bool {
        self.shared.tools_changed.swap(false, Ordering::SeqCst)
    }
    pub(crate) fn mark_tools_changed(&self) {
        self.shared.tools_changed.store(true, Ordering::SeqCst);
    }
    pub(crate) fn alive(&self) -> bool {
        self.shared.alive.load(Ordering::SeqCst)
    }
    pub(crate) fn stderr(&self) -> String {
        self.shared.stderr()
    }
    pub(crate) fn redact(&self, text: &str) -> String {
        self.shared.redactor.text(text)
    }
    fn send(&self, message: Value) -> Result<()> {
        self.outgoing
            .lock()
            .unwrap()
            .as_ref()
            .ok_or_else(|| anyhow!("server exited; outcome unknown"))?
            .send(message.to_string())
            .map_err(|_| anyhow!("server exited; outcome unknown"))
    }
    pub(crate) fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send(json!({"jsonrpc":"2.0","method":method,"params":params}))
    }
    pub(crate) async fn request(self: &Arc<Self>, method: &str, params: Value) -> Result<Value> {
        let id = self.shared.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        self.shared.pending.lock().unwrap().insert(id, sender);
        let mut guard = RequestGuard {
            connection: self.clone(),
            id,
            armed: true,
        };
        // Insert before checking alive so an exit cannot strand a request.
        if !self.alive() {
            bail!("server exited; outcome unknown");
        }
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
        let reply = receiver
            .await
            .map_err(|_| anyhow!("server exited; outcome unknown"))?;
        guard.armed = false;
        reply.map_err(|message| anyhow!(message))
    }
    pub(crate) fn stop(&self, grace: Duration) {
        self.shared.fail_all("MCP host shut down; outcome unknown");
        self.shared.close_input.cancel();
        self.outgoing.lock().unwrap().take();
        if let Some(sender) = self.stop.lock().unwrap().take() {
            if grace.is_zero() && !self.shared.done.is_cancelled() {
                self.tree.kill();
            }
            let _ = sender.send(grace);
        }
    }
    pub(crate) async fn shutdown(&self, grace: Duration) {
        self.stop(grace);
        let task = self.supervisor.lock().unwrap().take();
        if let Some(task) = task {
            let _ = task.await;
        } else {
            self.shared.done.cancelled().await;
        }
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.stop(Duration::ZERO);
    }
}

/// Dispatch cancellation drops the invoke future. No async destructor is
/// needed: removing the waiter and queueing this notification is synchronous.
struct RequestGuard {
    connection: Arc<Connection>,
    id: u64,
    armed: bool,
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        if self.armed {
            self.connection
                .shared
                .pending
                .lock()
                .unwrap()
                .remove(&self.id);
            let _ = self.connection.notify(
                "notifications/cancelled",
                json!({"requestId":self.id,"reason":"Call abandoned; outcome unknown"}),
            );
        }
    }
}

async fn read_frame(
    reader: &mut (impl AsyncBufRead + Unpin),
    limit: usize,
) -> Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                bail!("unterminated MCP frame")
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let length = newline.map_or(available.len(), |offset| offset + 1);
        if frame.len() + length > limit {
            bail!("MCP frame exceeds size limit");
        }
        frame.extend_from_slice(&available[..length]);
        reader.consume(length);
        if newline.is_some() {
            return Ok(Some(frame));
        }
    }
}

async fn drain_stderr(stream: impl AsyncRead + Unpin, shared: Arc<Shared>) {
    use tokio::io::AsyncReadExt;
    let mut stream = stream;
    let mut bytes = [0; 1024];
    let mut pending = Vec::new();
    while let Ok(count) = stream.read(&mut bytes).await {
        pending.extend_from_slice(&bytes[..count]);
        let safe = shared.redactor.prefix(&mut pending, count == 0);
        let mut ring = shared.stderr.lock().unwrap();
        ring.extend(safe);
        while ring.len() > STDERR_LIMIT {
            ring.pop_front();
        }
        if count == 0 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_covers_json_strings_and_keys() {
        let redactor = Redactor(vec!["private\"token".into()]);
        let mut value = json!({"private\"token": ["prefix private\"token suffix"]});
        redactor.value(&mut value);
        assert_eq!(value, json!({"[redacted]": ["prefix [redacted] suffix"]}));
    }

    #[test]
    fn streaming_redaction_withholds_secrets_across_chunks_and_ring_boundaries() {
        let redactor = Redactor(vec!["secret-longer-than-a-read".into()]);
        let mut pending = b"hello secret-longer".to_vec();
        let mut output = redactor.prefix(&mut pending, false);
        assert!(output.is_empty());
        pending.extend_from_slice(b"-than-a-read world");
        output.extend(redactor.prefix(&mut pending, false));
        output.extend(redactor.prefix(&mut pending, true));
        assert_eq!(String::from_utf8(output).unwrap(), "hello [redacted] world");
        let secret = "X".repeat(STDERR_LIMIT * 2);
        let redactor = Redactor(vec![secret.clone()]);
        let mut pending = secret.into_bytes();
        assert!(
            redactor
                .prefix(&mut pending, false)
                .starts_with(b"[redacted]")
        );
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn newline_frames_have_a_memory_bound() {
        let mut reader = BufReader::new(&b"123456789\n"[..]);
        assert!(read_frame(&mut reader, 5).await.is_err());
        let mut reader = BufReader::new(&b"{}\n{}\n"[..]);
        assert_eq!(read_frame(&mut reader, 5).await.unwrap().unwrap(), b"{}\n");
        assert_eq!(read_frame(&mut reader, 5).await.unwrap().unwrap(), b"{}\n");
        assert!(read_frame(&mut reader, 5).await.unwrap().is_none());
    }
}
