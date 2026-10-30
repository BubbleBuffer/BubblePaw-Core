//! Local CLI attachment. The Store lease elects the owner; it outlives this host.
use crate::cli_host::*;
use bubblepaw_runner::{
    cli_output::{Record, Stream},
    delegate::{assignment::Assignment, routing::RouteChoice},
    host_transport::{Connection, HostEndpoint},
};
use tokio::io::{AsyncWrite, BufReader};

mod io;
mod sessions;
use io::{interrupt_listener, read_json, write_json};
use sessions::{CliContext, session_request};
pub use sessions::{SessionSource, serve_embedded};

const CONNECT_WAIT: Duration = Duration::from_secs(5);

#[derive(Clone, Serialize, Deserialize)]
pub struct PreparedRequest {
    pub cli: Cli,
    pub effective: EffectiveConfig,
    pub delegation: Option<(Assignment, RouteChoice)>,
    pub task_prompt: Option<String>,
    pub workspace: PathBuf,
    pub data_dir: PathBuf,
    pub legacy_db: PathBuf,
}

pub fn is_owned(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .to_string()
            .contains("already owned by another host process")
    })
}

pub fn ownership_error(data_dir: &std::path::Path, error: anyhow::Error) -> anyhow::Error {
    let pid = std::fs::read(data_dir.join("host.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| value["pid"].as_u64());
    match pid {
        Some(pid) => error.context(format!("Data directory is owned by host pid {pid}")),
        None => error.context("Data directory is owned by another host (pid not yet published)"),
    }
}

pub async fn attach(mut request: PreparedRequest, connection: Connection) -> Result<()> {
    request.effective.prepare_for_attach();
    let json_output = request.cli.json;
    let is_run = matches!(request.cli.command, Command::Run { .. });
    let (read, mut write) = tokio::io::split(connection);
    let mut reader = BufReader::new(read);
    // Registered before the submission is written: Tokio delivers a signal only to
    // listeners that already exist, so a SIGINT before the first poll would otherwise
    // take the default action and kill the client without sending `run.cancel`.
    let mut interrupt = interrupt_listener().context("could not listen for Ctrl-C")?;
    // Once submission begins, never reconnect or retry. The host may already
    // have accepted input or dispatched an external effect.
    write_json(
        &mut write,
        &json!({"jsonrpc":"2.0","id":1,"method":"attach.submit","params":request}),
    )
    .await
    .context("host connection interrupted during submission; request was not replayed")?;
    let output = Output::default();
    let mut session = None::<String>;
    let mut cancelled = false;
    let mut final_run_record = false;
    let mut import_wait_printed = false;
    loop {
        let next = read_json(&mut reader);
        tokio::pin!(next);
        let incoming = tokio::select! {
            result = &mut next => result,
            _ = interrupt.recv(), if !cancelled => {
                cancelled = true;
                if write_json(&mut write, &json!({"jsonrpc":"2.0","method":"run.cancel","params":{}})).await.is_err() {
                    break;
                }
                next.await
            }
        };
        let record = match incoming {
            Ok(Some(record)) => record,
            Ok(None) | Err(_) => break,
        };
        ensure!(record["jsonrpc"] == "2.0", "invalid host protocol response");
        match record["method"].as_str() {
            Some("event") => {
                let event: HarnessEvent = serde_json::from_value(record["params"].clone())?;
                session.get_or_insert_with(|| event.session_id.clone());
                output.event(&event, json_output)?;
            }
            Some("host.importing") => {
                if !import_wait_printed {
                    import_wait_printed = true;
                    eprintln!("{IMPORT_WAIT_NOTICE}");
                }
            }
            Some("host.retry") if session.is_none() => return Err(anyhow::Error::new(HostRetry)),
            Some("attach.output") => {
                let stream: Stream = serde_json::from_value(record["params"]["stream"].clone())?;
                let text = record["params"]["text"]
                    .as_str()
                    .context("missing output text")?;
                if matches!(stream, Stream::Stdout)
                    && text.lines().any(|line| {
                        serde_json::from_str::<Value>(line).is_ok_and(|v| v["type"] == "result")
                    })
                {
                    final_run_record = true;
                }
                output.write(stream, text.to_owned())?;
            }
            _ if record["id"] == 1 => {
                if let Some(error) = record.get("error") {
                    bail!(
                        "{}",
                        error["message"]
                            .as_str()
                            .unwrap_or("host rejected submission")
                    );
                }
                if record["result"]["exit_code"] == 0 {
                    return Ok(());
                }
                // Host sends the exact standalone diagnostic text via stderr.
                // Return an empty typed failure to avoid printing it twice.
                return Err(anyhow::Error::new(RemoteFailure));
            }
            _ => bail!("unexpected host protocol record"),
        }
    }
    if json_output && is_run && !final_run_record {
        output.json(&json!({"type":"result","session_id":session,"run":{"status":"interrupted","error":"host connection lost; request was not replayed"}}))?;
    }
    bail!("run interrupted: host connection lost; request was not replayed")
}

/// The client line printed while a host imports legacy stores.
pub const IMPORT_WAIT_NOTICE: &str = "waiting for the host to finish importing legacy stores";

/// The host imported legacy stores and stops without serving (a one-shot
/// command). The request was never read, so the client may elect again.
#[derive(Debug)]
pub struct HostRetry;
impl std::fmt::Display for HostRetry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("host stopped after importing legacy stores; request was not submitted")
    }
}
impl std::error::Error for HostRetry {}

/// A host that published its endpoint before importing legacy stores, with
/// the clients that attached during the import. Their submissions are unread.
pub struct ImportingHost {
    endpoint: HostEndpoint,
    waiting: Vec<Connection>,
}

impl ImportingHost {
    /// Tell every waiting client to elect again, then stop listening.
    pub async fn release(self) {
        let Self { endpoint, waiting } = self;
        drop(endpoint);
        for mut connection in waiting {
            let _ = write_json(
                &mut connection,
                &json!({"jsonrpc":"2.0","method":"host.retry","params":{}}),
            )
            .await;
        }
    }
}

/// Publish the host endpoint, then import legacy stores on a blocking thread.
/// Clients that attach meanwhile are told once that the host is importing
/// and wait; they are served (or released) after the import.
pub async fn import_legacy_stores(
    store: &Store,
    data_dir: &std::path::Path,
) -> Result<ImportingHost> {
    let worker_store = store.clone();
    import_while_accepting(data_dir, move || {
        wait_for_test_gate();
        bubblepaw_runner::legacy_import::import_legacy_stores(&worker_store)
    })
    .await
}

/// Test hook, debug builds only: hold the import until the file named by
/// `BPBASE_TEST_IMPORT_GATE` exists, so tests can attach during it.
#[cfg(debug_assertions)]
fn wait_for_test_gate() {
    let Some(gate) = std::env::var_os("BPBASE_TEST_IMPORT_GATE") else {
        return;
    };
    let gate = PathBuf::from(gate);
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while !gate.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(not(debug_assertions))]
fn wait_for_test_gate() {}

async fn import_while_accepting(
    data_dir: &std::path::Path,
    import: impl FnOnce() -> Result<bubblepaw_runner::legacy_import::CentralImport> + Send + 'static,
) -> Result<ImportingHost> {
    let mut endpoint = HostEndpoint::bind(data_dir).await?;
    let worker = tokio::task::spawn_blocking(import);
    tokio::pin!(worker);
    let mut waiting = Vec::new();
    let mut listener_error = None;
    let imported = loop {
        tokio::select! {
            result = &mut worker => break result,
            accepted = endpoint.accept(), if listener_error.is_none() => match accepted {
                Ok(mut connection) => {
                    let notice = json!({"jsonrpc":"2.0","method":"host.importing","params":{}});
                    if write_json(&mut connection, &notice).await.is_ok() {
                        waiting.push(connection);
                    }
                }
                Err(error) => listener_error = Some(error),
            },
        }
    };
    let import = imported.context("legacy import stopped")??;
    for notice in bubblepaw_runner::legacy_import::notices(&import) {
        eprintln!("{notice}");
    }
    if let Some(error) = listener_error {
        return Err(error.context("host control listener failed during legacy import"));
    }
    Ok(ImportingHost { endpoint, waiting })
}

#[derive(Debug)]
pub struct RemoteFailure;
impl std::fmt::Display for RemoteFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("attached request failed")
    }
}
impl std::error::Error for RemoteFailure {}

pub async fn serve(
    request: PreparedRequest,
    store: Store,
    importing: Option<ImportingHost>,
) -> Result<()> {
    let (mut endpoint, waiting) = match importing {
        Some(ImportingHost { endpoint, waiting }) => (endpoint, waiting),
        None => (HostEndpoint::bind(&request.data_dir).await?, Vec::new()),
    };
    let lifetime = HostLifetime::default();
    let host_stop = CancellationToken::new();
    let cancel = CancellationToken::new();
    let context = CliContext::default();
    let sessions = context.source(&request, &store, &lifetime);
    // Heap pin the request boundary: pin! would keep the complete execution
    // state (including every command branch) inline in the host future.
    let mut primary = context.primary(request, &store, &lifetime, &cancel).await?;
    let mut primary_result = None;
    let mut clients = tokio::task::JoinSet::new();
    // One registered listener for the whole loop: a Ctrl-C must cancel the
    // primary work even when it arrives between two polls of the select.
    let mut interrupt = interrupt_listener().context("could not listen for Ctrl-C")?;
    // Clients that attached during the legacy import submit now.
    for connection in waiting {
        let store = store.clone();
        let lifetime = lifetime.clone();
        let sessions = sessions.clone();
        let cancellation = host_stop.clone();
        clients.spawn(async move {
            connection_request(connection, store, lifetime, sessions, cancellation).await
        });
    }
    loop {
        if primary_result.is_some() && clients.is_empty() {
            break;
        }
        tokio::select! {
            biased;
            accepted = endpoint.accept() => {
                match accepted {
                    Ok(connection) => {
                        let store = store.clone();
                        let lifetime = lifetime.clone();
                        let sessions = sessions.clone();
                        let cancellation = host_stop.clone();
                        clients.spawn(async move { connection_request(connection, store, lifetime, sessions, cancellation).await });
                    }
                    Err(error) => {
                        // Fatal listener loss stops every request. Ordinary
                        // primary completion/Ctrl-C still lets clients drain.
                        drop(endpoint);
                        host_stop.cancel();
                        cancel.cancel();
                        // Cancel shared tool/provider work before waiting for
                        // HTTP handlers and request cleanup to drain.
                        let _ = lifetime.shutdown().await;
                        if primary_result.is_none() { let _ = primary.as_mut().await; }
                        while clients.join_next().await.is_some() {}
                        let _ = context.shutdown().await;
                        return Err(error.context("host control listener failed"));
                    }
                }
            }
            result = &mut primary, if primary_result.is_none() => primary_result = Some(result),
            _ = clients.join_next(), if !clients.is_empty() => {}
            _ = interrupt.recv(), if primary_result.is_none() && !cancel.is_cancelled() => cancel.cancel(),
        }
    }
    // Removing discovery happens before dropping the final Store lease. A new
    // owner cannot have its host.json removed by this host's destructor.
    drop(endpoint);
    // Sessions have ended; the context's request cleanup runs before the
    // shared runtime stops.
    let context_stopped = context.shutdown().await;
    let stopped = lifetime.shutdown().await;
    primary_result.expect("primary completed")?;
    context_stopped?;
    stopped
}

async fn connection_request(
    connection: Connection,
    store: Store,
    lifetime: HostLifetime,
    sessions: SessionSource,
    host_stop: CancellationToken,
) -> Result<()> {
    let cancel = CancellationToken::new();
    let (read, mut write) = tokio::io::split(connection);
    let mut reader = BufReader::new(read);
    let incoming = tokio::select! {
        _ = host_stop.cancelled() => return Ok(()),
        incoming = tokio::time::timeout(CONNECT_WAIT, read_json(&mut reader)) => incoming??,
    };
    let Some(incoming) = incoming else {
        return Ok(());
    };
    let id = incoming.get("id").cloned().unwrap_or(Value::Null);
    if incoming["jsonrpc"] == "2.0" && incoming["method"] == "attach.session" {
        return session_request(id, reader, write, sessions, host_stop).await;
    }
    let parsed = (|| -> Result<PreparedRequest> {
        ensure!(
            incoming["jsonrpc"] == "2.0" && incoming["method"] == "attach.submit",
            "expected attach.submit"
        );
        ensure!(id.is_string() || id.is_number(), "submission needs an id");
        let mut request: PreparedRequest = serde_json::from_value(incoming["params"].clone())?;
        ensure!(
            matches!(
                request.cli.command,
                Command::Run { .. } | Command::Delegate { .. }
            ),
            "only run and delegate support attach"
        );
        ensure!(
            !matches!(
                request.cli.command,
                Command::Run {
                    no_session: true,
                    ..
                }
            ),
            "temporary runs must use their private store"
        );
        ensure!(
            !matches!(request.cli.command, Command::Run { .. }) || request.task_prompt.is_some(),
            "run requires a prompt"
        );
        ensure!(
            !matches!(request.cli.command, Command::Delegate { .. })
                || request.delegation.is_some(),
            "delegate requires an assignment"
        );
        let data_dir = store.data_dir().context("host has no data directory")?;
        ensure!(
            request.data_dir.canonicalize()? == data_dir,
            "request data directory differs from host"
        );
        request.data_dir = data_dir.to_owned();
        request.workspace = request.workspace.canonicalize()?;
        ensure!(request.workspace.is_dir(), "workspace must be a directory");
        ensure!(
            request.effective.load_options().workspace.canonicalize()? == request.workspace,
            "configuration workspace mismatch"
        );
        request.effective.settings()?;
        Ok(request)
    })();
    let request = match parsed {
        Ok(request) => request,
        Err(error) => {
            write_json(&mut write, &json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":error.to_string()}})).await?;
            return Ok(());
        }
    };
    let (send, mut records) = mpsc::channel(256);
    let output = Output::remote(send, cancel.clone());
    let mut execution = Box::pin(session_host::execute(
        request,
        store,
        lifetime,
        output.clone(),
        cancel.clone(),
        None,
    ));
    let mut disconnected = false;
    let next = read_json(&mut reader);
    tokio::pin!(next);
    let mut read_done = false;
    let outcome = loop {
        tokio::select! {
            result = &mut execution => break result,
            _ = host_stop.cancelled(), if !cancel.is_cancelled() => cancel.cancel(),
            record = records.recv() => {
                if let Some(record) = record
                    && !disconnected && send_output(&mut write, record).await.is_err() {
                    disconnected = true;
                    cancel.cancel();
                }
            }
            incoming = &mut next, if !disconnected && !read_done => {
                read_done = true;
                match incoming {
                    Ok(Some(record)) if record["jsonrpc"] == "2.0" && record["method"] == "run.cancel" => cancel.cancel(),
                    _ => { disconnected = true; cancel.cancel(); }
                }
            }
        }
    };
    while let Ok(record) = records.try_recv() {
        if !disconnected && send_output(&mut write, record).await.is_err() {
            disconnected = true;
        }
    }
    if disconnected {
        return Ok(());
    }
    if let Err(error) = &outcome {
        send_output(
            &mut write,
            Record::Text {
                stream: Stream::Stderr,
                text: format!("Error: {error:?}\n"),
            },
        )
        .await?;
    }
    write_json(&mut write, &json!({"jsonrpc":"2.0","id":id,"result":{"exit_code":if outcome.is_ok() {0} else {1},"status":if cancel.is_cancelled() {"cancelled"} else if outcome.is_ok() {"completed"} else {"failed"}}})).await
}

async fn send_output<W: AsyncWrite + Unpin>(writer: &mut W, record: Record) -> Result<()> {
    let record = match record {
        Record::Text { stream, text } => {
            json!({"jsonrpc":"2.0","method":"attach.output","params":{"stream":stream,"text":text}})
        }
        Record::Event(event) => json!({"jsonrpc":"2.0","method":"event","params":event}),
    };
    // A client that neither consumes output nor disconnects cannot hold a host forever.
    write_json(writer, &record).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fatal_host_stop_cancels_idle_connection_without_submitting() -> Result<()> {
        let (server, _client) = tokio::io::duplex(1024);
        let host_stop = CancellationToken::new();
        let request = tokio::spawn(connection_request(
            Box::new(server),
            Store::in_memory()?,
            HostLifetime::default(),
            Arc::new(|| Box::pin(async { anyhow::bail!("no host context") })),
            host_stop.clone(),
        ));
        tokio::task::yield_now().await;
        host_stop.cancel();
        tokio::time::timeout(Duration::from_secs(1), request).await???;
        Ok(())
    }
}
