//! The two transports under test. Each is the real binary started against the
//! same config and data directory; only one process owns the store at a time.
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

const TIMEOUT: Duration = Duration::from_secs(60);

/// Same offline shape as the stdio parity fixtures: one scripted provider,
/// no summarization and no automatic wake-ups. JavaScript mode is the default
/// because script runs (todo and question fixtures) synthesize a `js` call, and
/// native mode removes `js` from the tool surface. Sessions bind the configured
/// mode at their first accepted input, so both transports bind the same mode.
const CONFIG: &str = "\
[model]
provider='demo'
main='demo/first'
[providers.demo]
kind='scripted'
models=[{id='first'},{id='second'}]
[agent]
execution_mode='js'
[context]
auto_summarize=false
[agents]
auto_wake=false
";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// `bubblepaw stdio`: JSON-RPC lines on stdin and stdout.
    Stdio,
    /// `bubblepaw web`: `POST /rpc` on the loopback listener.
    Rpc,
}

pub fn seed(root: &Path) {
    std::fs::write(root.join("config.toml"), CONFIG).unwrap();
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).unwrap();
    std::fs::write(assets.join("index.html"), "<html>host api parity</html>").unwrap();
}

pub struct Peer {
    transport: Transport,
    next: u64,
    child: Child,
    io: Io,
}

enum Io {
    Stdio {
        input: ChildStdin,
        output: BufReader<ChildStdout>,
    },
    Rpc {
        base: String,
        client: reqwest::Client,
    },
}

/// The arguments both binaries share; the subcommand is appended by `start`.
fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bubblepaw"));
    command
        .arg("--config")
        .arg(root.join("config.toml"))
        .arg("--workspace")
        .arg(root)
        .arg("--data-dir")
        .arg(root.join("data"))
        .kill_on_drop(true);
    for (name, _) in std::env::vars() {
        if name.starts_with("BUBBLEPAW_") {
            command.env_remove(name);
        }
    }
    command
}

impl Peer {
    pub async fn start(transport: Transport, root: &Path) -> Self {
        match transport {
            Transport::Stdio => {
                let mut child = command(root)
                    .arg("stdio")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap();
                let input = child.stdin.take().unwrap();
                let output = BufReader::new(child.stdout.take().unwrap());
                Self {
                    transport,
                    next: 1,
                    child,
                    io: Io::Stdio { input, output },
                }
            }
            Transport::Rpc => {
                let (child, base) =
                    crate::web_host::start(command(root), &root.join("assets")).await;
                Self {
                    transport,
                    next: 1,
                    child,
                    io: Io::Rpc {
                        base,
                        client: reqwest::Client::new(),
                    },
                }
            }
        }
    }

    /// One request and its reply. The reply must carry this request's id.
    pub async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        self.call_with_id(json!(id), method, params).await
    }

    /// Fixture IDs remain stable even when the transports need different polls.
    pub async fn call_with_id(&mut self, id: Value, method: &str, params: Value) -> Value {
        let frame = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let bytes = serde_json::to_vec(&frame).unwrap();
        let reply = tokio::time::timeout(TIMEOUT, self.exchange(&bytes))
            .await
            .unwrap_or_else(|_| panic!("{:?} {method} timed out", self.transport))
            .unwrap_or_else(|| panic!("{:?} {method} got no reply", self.transport));
        assert_eq!(
            reply["id"], id,
            "{:?} {method}: reply does not answer this request: {reply}",
            self.transport
        );
        reply
    }

    /// Send one raw frame exactly as received. An answered frame returns its
    /// reply: HTTP 200 on `/rpc`. A notification returns None: HTTP 204 on `/rpc`,
    /// nothing read on stdio.
    pub async fn raw(&mut self, bytes: &[u8], answer: bool) -> Option<Value> {
        match &mut self.io {
            Io::Stdio { input, output } => {
                write_line(input, bytes).await;
                if answer {
                    tokio::time::timeout(TIMEOUT, read_response(output))
                        .await
                        .expect("stdio reply timed out")
                } else {
                    None
                }
            }
            Io::Rpc { base, client } => {
                let (status, reply) = post(client, base, bytes).await;
                let expected = if answer { 200 } else { 204 };
                assert_eq!(
                    status.as_u16(),
                    expected,
                    "/rpc status for a frame that {} answered",
                    if answer { "must be" } else { "must not be" }
                );
                reply
            }
        }
    }

    async fn exchange(&mut self, bytes: &[u8]) -> Option<Value> {
        match &mut self.io {
            Io::Stdio { input, output } => {
                write_line(input, bytes).await;
                read_response(output).await
            }
            Io::Rpc { base, client } => {
                let (status, reply) = post(client, base, bytes).await;
                assert_eq!(
                    status.as_u16(),
                    200,
                    "/rpc answered a request with {status}"
                );
                reply
            }
        }
    }

    /// Call after the `shutdown` step. Stdio must exit on its own with success.
    /// `/rpc` answers `shutdown` but does not stop the web host, so this peer
    /// stops only the child it started, through that child's own handle.
    pub async fn finish(self) {
        let Self {
            transport,
            mut child,
            ..
        } = self;
        match transport {
            Transport::Stdio => {
                let status = tokio::time::timeout(TIMEOUT, child.wait())
                    .await
                    .unwrap_or_else(|_| panic!("{transport:?} did not exit after shutdown"))
                    .unwrap();
                assert!(status.success(), "{transport:?} exited with {status}");
            }
            Transport::Rpc => {
                // Already-exited children report an error here, which is fine.
                let _ = child.start_kill();
                tokio::time::timeout(TIMEOUT, child.wait())
                    .await
                    .unwrap_or_else(|_| panic!("{transport:?} did not stop after kill"))
                    .unwrap();
            }
        }
    }
}

async fn write_line(input: &mut ChildStdin, bytes: &[u8]) {
    input.write_all(bytes).await.unwrap();
    input.write_all(b"\n").await.unwrap();
    input.flush().await.unwrap();
}

/// The next reply line. Server notifications carry a `method` and are skipped;
/// replies, including the id-less error reply to a bad frame, have none.
async fn read_response(output: &mut BufReader<ChildStdout>) -> Option<Value> {
    loop {
        let mut line = String::new();
        if output.read_line(&mut line).await.unwrap() == 0 {
            return None;
        }
        let value: Value = serde_json::from_str(&line)
            .unwrap_or_else(|_| panic!("stdio wrote a non-JSON line: {line}"));
        if value.get("method").is_none() {
            return Some(value);
        }
    }
}

/// `POST /rpc` with the frame as bytes. An empty body is None; a body that is
/// not JSON comes back as a string so the comparison shows it.
pub async fn post(
    client: &reqwest::Client,
    base: &str,
    bytes: &[u8],
) -> (reqwest::StatusCode, Option<Value>) {
    let response = client
        .post(format!("{base}/rpc"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap();
    let status = response.status();
    let text = response.text().await.unwrap();
    if text.trim().is_empty() {
        return (status, None);
    }
    let value = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, Some(value))
}
