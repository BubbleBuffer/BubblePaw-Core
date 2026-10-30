//! The transports under test. Stdio, `/rpc` and the attached client use the real
//! binary started against the same config and data directory; the embedded
//! client runs that host context in this process. Only one owns the store at a
//! time.
use bubblepaw_runner::host_client::{ClientError, HostClient};
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
    /// A host-API client attached to `bubblepaw stdio`'s control endpoint.
    Attached,
    /// A host-API client on the CLI host context, built in this process.
    Embedded,
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
    child: Option<Child>,
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
    Client {
        client: HostClient,
        /// The attached host's stdin; closing it stops `bubblepaw stdio`.
        input: Option<ChildStdin>,
        /// The embedded host context and the runtime it registered in.
        host: Option<Box<(HostContext, HostLifetime)>>,
    },
}

type HostContext = bubblepaw_runner::session_host::host_context::HostContext;
type HostLifetime = bubblepaw_runner::host_lifetime::HostLifetime;

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
                    child: Some(child),
                    io: Io::Stdio { input, output },
                }
            }
            Transport::Rpc => {
                let (child, base) =
                    crate::web_host::start(command(root), &root.join("assets")).await;
                Self {
                    transport,
                    next: 1,
                    child: Some(child),
                    io: Io::Rpc {
                        base,
                        client: reqwest::Client::new(),
                    },
                }
            }
            Transport::Attached => {
                let mut child = command(root)
                    .arg("stdio")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap();
                let input = child.stdin.take();
                let client = HostClient::attach(&root.join("data"), TIMEOUT)
                    .await
                    .unwrap();
                Self {
                    transport,
                    next: 1,
                    child: Some(child),
                    io: Io::Client {
                        client,
                        input,
                        host: None,
                    },
                }
            }
            Transport::Embedded => {
                let (context, lifetime) = embedded_host(root).await;
                let client = HostClient::in_process(context.ctx().clone());
                Self {
                    transport,
                    next: 1,
                    child: None,
                    io: Io::Client {
                        client,
                        input: None,
                        host: Some(Box::new((context, lifetime))),
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
        if let Io::Client { client, .. } = &self.io {
            let reply = tokio::time::timeout(TIMEOUT, client.call(method, params))
                .await
                .unwrap_or_else(|_| panic!("{:?} {method} timed out", self.transport));
            // The client returns the result; the envelope is rebuilt to compare.
            return match reply {
                Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
                Err(ClientError::Rpc {
                    code,
                    message,
                    data,
                }) => {
                    let mut error = json!({"code":code,"message":message});
                    if let Some(data) = data {
                        error["data"] = data;
                    }
                    json!({"jsonrpc":"2.0","id":id,"error":error})
                }
                Err(error) => panic!("{:?} {method}: {error}", self.transport),
            };
        }
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
            Io::Client { .. } => panic!("the host-API client sends no raw frames"),
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
            Io::Client { .. } => unreachable!("client calls do not exchange frames"),
        }
    }

    /// Call after the `shutdown` step. Stdio must exit on its own with success.
    /// `/rpc` answers `shutdown` but does not stop the web host, so this peer
    /// stops only the child it started, through that child's own handle.
    pub async fn finish(self) {
        let Self {
            transport,
            child,
            io,
            ..
        } = self;
        if let Io::Client {
            client,
            input,
            host,
        } = io
        {
            client.close().await;
            // End of stdin ends the attached host's own stdio command.
            drop(input);
            if let Some(host) = host {
                let (context, lifetime) = *host;
                context.shutdown().await.unwrap();
                lifetime.shutdown().await.unwrap();
            }
        }
        let Some(mut child) = child else { return };
        match transport {
            Transport::Embedded => unreachable!("the embedded host has no child"),
            Transport::Stdio | Transport::Attached => {
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

/// The CLI host context for `stdio` with the shared config, run in this
/// process: the request a `bubblepaw stdio` host builds, without the CLI's
/// overrides and user config, which these runs do not use.
async fn embedded_host(root: &Path) -> (HostContext, HostLifetime) {
    use bubblepaw_runner::{
        cli::Cli,
        config::{EffectiveConfig, LoadOptions},
        host_attach::PreparedRequest,
    };
    use clap::Parser;
    let workspace = root.canonicalize().unwrap();
    let config = root.join("config.toml");
    let data_dir = std::path::absolute(root.join("data")).unwrap();
    let cli = Cli::parse_from([
        std::ffi::OsStr::new("bubblepaw"),
        "--config".as_ref(),
        config.as_os_str(),
        "--workspace".as_ref(),
        workspace.as_os_str(),
        "--data-dir".as_ref(),
        data_dir.as_os_str(),
        "stdio".as_ref(),
    ]);
    let mut effective = EffectiveConfig::layer(LoadOptions {
        workspace: workspace.clone(),
        home: None,
        custom_file: Some(config),
        env: Vec::new(),
        cli: Vec::new(),
    })
    .unwrap();
    effective.anchor_paths(&std::env::current_dir().unwrap());
    let store = bubblepaw_core::Store::open(&data_dir).unwrap();
    bubblepaw_runner::spill::SpillStore::new(&data_dir)
        .unwrap()
        .attach(&store);
    let request = PreparedRequest {
        cli,
        effective,
        delegation: None,
        task_prompt: None,
        workspace,
        legacy_db: data_dir.join("archive.sqlite"),
        data_dir,
    };
    let lifetime = HostLifetime::default();
    let context = HostContext::start(request, store, lifetime.clone(), None)
        .await
        .unwrap();
    (context, lifetime)
}
