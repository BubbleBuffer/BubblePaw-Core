//! Header secrets stored with `McpHost::set_header`: sent by the HTTP transport
//! next to `headers_from` (the stored value wins), gone after removal, kept in the
//! per-server secret file only, and never visible in status, notifications or
//! errors. A local fake HTTP MCP server records every request's headers.
use bubblepaw_mcp::{McpClientEnv, McpHost, McpHostOptions, McpServerSettings, McpSettings};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

const ENV_NAME: &str = "BUBBLEPAW_HEADERS_TEST_API_KEY";
const HEADER: &str = "X-Api-Key";
const FROM_ENV: &str = "synthetic-env-value-only-for-this-fixture";
const STORED: &str = "synthetic-stored-value-only-for-this-fixture";
const OTHER: &str = "synthetic-second-value-only-for-this-fixture";

/// A modern-protocol server: `tools/list` lists one tool. Each request's
/// lowercase headers are recorded.
struct Fake {
    url: String,
    seen: Arc<Mutex<Vec<BTreeMap<String, String>>>>,
    task: JoinHandle<()>,
}
impl Fake {
    async fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let task = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let recorded = recorded.clone();
                tokio::spawn(async move {
                    let _ = serve(socket, recorded).await;
                });
            }
        });
        Self { url, seen, task }
    }
    fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
    /// The value of `name` on the requests after the first `from`.
    fn values(&self, name: &str, from: usize) -> Vec<Option<String>> {
        self.seen.lock().unwrap()[from..]
            .iter()
            .map(|headers| headers.get(&name.to_ascii_lowercase()).cloned())
            .collect()
    }
}
impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(
    mut socket: TcpStream,
    seen: Arc<Mutex<Vec<BTreeMap<String, String>>>>,
) -> std::io::Result<()> {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    let end = loop {
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let text = String::from_utf8_lossy(&bytes[..end]).into_owned();
    let headers: BTreeMap<String, String> = text
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    while bytes.len() < end + length {
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    let body: Value = serde_json::from_slice(&bytes[end..end + length]).unwrap_or(Value::Null);
    seen.lock().unwrap().push(headers);
    let (status, reply) = match body["method"].as_str() {
        Some("tools/list") => (
            200,
            json!({"jsonrpc":"2.0","id":body["id"],"result":{"tools":[
                {"name":"echo","description":"fixture","inputSchema":{"type":"object"}}
            ]}})
            .to_string(),
        ),
        _ => (405, String::new()),
    };
    let head = format!(
        "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reply.len()
    );
    socket.write_all(head.as_bytes()).await?;
    socket.write_all(reply.as_bytes()).await?;
    socket.shutdown().await
}

fn settings(url: &str, oauth: bool) -> McpSettings {
    let mut config = McpServerSettings {
        url: Some(url.into()),
        oauth,
        startup_timeout_secs: 3,
        call_timeout_secs: 2,
        ..Default::default()
    };
    if !oauth {
        config.headers_from = BTreeMap::from([(HEADER.into(), ENV_NAME.into())]);
    }
    McpSettings {
        servers: BTreeMap::from([("fixture".into(), config)]),
        ..Default::default()
    }
}

async fn start(settings: &McpSettings, root: &Path) -> McpHost {
    McpHost::start_with_options(
        settings,
        root,
        McpHostOptions {
            client_env: Some(McpClientEnv::new([(ENV_NAME, FROM_ENV)])),
            auth_dir: Some(root.join("auth")),
        },
    )
    .await
}

fn headers(host: &McpHost) -> Vec<String> {
    host.servers()
        .into_iter()
        .find(|s| s.name == "fixture")
        .unwrap()
        .headers
}

fn secret_file(root: &Path) -> std::path::PathBuf {
    root.join("auth").join("mcp-fixture.json")
}

#[tokio::test]
async fn a_stored_header_is_sent_wins_over_headers_from_and_removal_stops_it() {
    let fake = Fake::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let host = start(&settings(&fake.url, false), root.path()).await;
    let mut receiver = host.subscribe();
    assert!(headers(&host).is_empty());
    // Only the headers_from value went out so far.
    let before = fake.count();
    assert!(before > 0);
    assert!(
        fake.values(HEADER, 0)
            .iter()
            .all(|v| v.as_deref() == Some(FROM_ENV))
    );

    // A stored value reconnects the server and wins for that header.
    host.set_header("fixture", HEADER, Some(STORED))
        .await
        .unwrap();
    assert_eq!(headers(&host), [HEADER]);
    assert!(fake.count() > before);
    assert!(
        fake.values(HEADER, before)
            .iter()
            .all(|v| v.as_deref() == Some(STORED))
    );
    let file = std::fs::read_to_string(secret_file(root.path())).unwrap();
    assert!(file.contains(STORED));

    // Setting again with another letter case replaces the value, not adds one.
    let second = fake.count();
    host.set_header("fixture", "x-api-key", Some(OTHER))
        .await
        .unwrap();
    assert_eq!(headers(&host).len(), 1);
    assert!(
        fake.values(HEADER, second)
            .iter()
            .all(|v| v.as_deref() == Some(OTHER))
    );

    // A second, unrelated stored header is sent too.
    let third = fake.count();
    host.set_header("fixture", "X-Other", Some(STORED))
        .await
        .unwrap();
    assert_eq!(headers(&host).len(), 2);
    assert!(
        fake.values("x-other", third)
            .iter()
            .all(|v| v.as_deref() == Some(STORED))
    );

    // Nothing visible carries a value: status, notifications, error text.
    let mut visible = vec![serde_json::to_string(&host.servers()).unwrap()];
    while let Ok(status) = receiver.try_recv() {
        visible.push(serde_json::to_string(&status).unwrap());
    }
    visible.push(host.warnings().join("\n"));
    let mut errors = Vec::new();
    for (server, header, value) in [
        ("missing", HEADER, Some(STORED)),
        ("fixture", "bad header", Some(STORED)),
        ("fixture", "Host", Some(STORED)),
        ("fixture", HEADER, Some("line\nbreak-synthetic-value")),
        ("fixture", HEADER, Some("")),
    ] {
        let error = host.set_header(server, header, value).await.unwrap_err();
        errors.push(format!("{error:#}"));
    }
    visible.extend(errors);
    for text in &visible {
        for value in [FROM_ENV, STORED, OTHER, "line\nbreak", "break-synthetic"] {
            assert!(!text.contains(value), "{value:?} leaked into {text}");
        }
    }

    // Removal drops it: the next connection sends the headers_from value again.
    let fourth = fake.count();
    host.set_header("fixture", HEADER, None).await.unwrap();
    assert_eq!(headers(&host), ["X-Other"]);
    assert!(
        fake.values(HEADER, fourth)
            .iter()
            .all(|v| v.as_deref() == Some(FROM_ENV))
    );
    host.set_header("fixture", "x-other", None).await.unwrap();
    assert!(headers(&host).is_empty());
    assert!(!secret_file(root.path()).exists());
    // Removing what is not stored is fine.
    host.set_header("fixture", "X-Other", None).await.unwrap();
    host.shutdown(Duration::ZERO).await;
}

#[tokio::test]
async fn stored_headers_survive_a_restart_and_removing_the_server_deletes_the_file() {
    let fake = Fake::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let host = start(&settings(&fake.url, false), root.path()).await;
    host.set_header("fixture", HEADER, Some(STORED))
        .await
        .unwrap();
    host.shutdown(Duration::ZERO).await;

    // A new host adopts the file: names listed, value sent.
    let before = fake.count();
    let host = start(&settings(&fake.url, false), root.path()).await;
    assert_eq!(headers(&host), [HEADER]);
    assert!(
        fake.values(HEADER, before)
            .iter()
            .all(|v| v.as_deref() == Some(STORED))
    );

    // Disabling keeps the secret; removing the entry deletes it.
    let mut disabled = settings(&fake.url, false);
    disabled.servers.get_mut("fixture").unwrap().enabled = false;
    host.reload(&disabled, None).await.unwrap();
    assert!(secret_file(root.path()).exists());
    host.reload(&McpSettings::default(), None).await.unwrap();
    assert!(!secret_file(root.path()).exists());
    assert!(host.servers().is_empty());
    host.shutdown(Duration::ZERO).await;
}

#[tokio::test]
async fn header_secrets_keep_oauth_tokens_and_refuse_authorization_with_oauth() {
    let fake = Fake::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let host = start(&settings(&fake.url, true), root.path()).await;
    let error = host
        .set_header("fixture", "Authorization", Some(STORED))
        .await
        .unwrap_err();
    assert!(!format!("{error:#}").contains(STORED));
    assert!(!secret_file(root.path()).exists());
    // Another header is fine next to OAuth; the server is simply not signed in.
    host.set_header("fixture", HEADER, Some(STORED))
        .await
        .unwrap();
    assert_eq!(headers(&host), [HEADER]);
    host.shutdown(Duration::ZERO).await;
}

#[tokio::test]
async fn stdio_servers_take_no_header_secrets() {
    let root = tempfile::tempdir().unwrap();
    let mut settings = McpSettings::default();
    settings.servers.insert(
        "fixture".into(),
        McpServerSettings {
            command: "unused".into(),
            enabled: false,
            ..Default::default()
        },
    );
    let host = start(&settings, root.path()).await;
    let error = host
        .set_header("fixture", HEADER, Some(STORED))
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("not an HTTP server"));
    host.shutdown(Duration::ZERO).await;
}
