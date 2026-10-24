//! In-process Streamable HTTP acceptance. No live server, credential, or model.
use bubblepaw_core::{Approval, CancellationToken, Tool, ToolContext, ToolResult};
use bubblepaw_mcp::{McpClientEnv, McpHost, McpServerSettings, McpSettings};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::{JoinHandle, JoinSet},
};

const MODERN: &str = "2026-07-28";
const LEGACY: &str = "2025-11-25";

#[tokio::test]
async fn authentication_rejections_and_redirects_do_not_initialize_or_follow_location() {
    for status in [401, 403, 307] {
        let fixture = Fixture::spawn(move |request| {
            Reply::status(status).header(
                "Location",
                &format!("http://{}/forwarded", request.header("host").unwrap()),
            )
        })
        .await;
        let root = tempfile::tempdir().unwrap();
        let host = start(fixture.config(), root.path()).await;
        assert!(host.tools().is_empty());
        let warning = host.warnings().join("\n");
        if status == 401 {
            assert!(
                warning.contains("authentication required")
                    && warning.contains("OAuth is not supported")
            );
        } else if status == 403 {
            assert!(warning.contains("forbidden"));
        } else {
            assert!(warning.contains("307"));
        }
        host.shutdown(Duration::ZERO).await;
        let requests = fixture.requests();
        assert_eq!(requests.len(), 1);
        assert_modern(&requests[0]);
        fixture.close().await;
    }
}

#[derive(Clone, Debug)]
struct Request {
    verb: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
    received: Instant,
}
impl Request {
    fn method(&self) -> &str {
        self.body["method"].as_str().unwrap_or("")
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

struct Reply {
    status: u16,
    content_type: &'static str,
    headers: Vec<(String, String)>,
    body: String,
    delay: Duration,
    disconnect: bool,
    truncate_body: bool,
    barrier: Option<Arc<tokio::sync::Barrier>>,
}
impl Reply {
    fn raw(status: u16, content_type: &'static str, body: String) -> Self {
        Self {
            status,
            content_type,
            headers: Vec::new(),
            body,
            delay: Duration::ZERO,
            disconnect: false,
            truncate_body: false,
            barrier: None,
        }
    }
    fn json(request: &Request, result: Value) -> Self {
        Self::raw(
            200,
            "application/json",
            json!({"jsonrpc":"2.0", "id":request.body["id"], "result":result}).to_string(),
        )
    }
    fn sse(request: &Request, result: Value) -> Self {
        Self::raw(
            200,
            "text/event-stream",
            format!(
                ": heartbeat\r\nevent: message\r\ndata: {}\r\n\r\n",
                json!({"jsonrpc":"2.0", "id":request.body["id"], "result":result})
            ),
        )
    }
    fn status(status: u16) -> Self {
        Self::raw(status, "application/json", String::new())
    }
    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers
            .retain(|(key, _)| !key.eq_ignore_ascii_case(name));
        self.headers.push((name.into(), value.into()));
        self
    }
    fn delayed(mut self) -> Self {
        self.delay = Duration::from_secs(10);
        self
    }
    fn disconnected() -> Self {
        Self {
            disconnect: true,
            ..Self::status(200)
        }
    }
    fn truncated(mut self) -> Self {
        self.truncate_body = true;
        self
    }
}

struct Fixture {
    url: String,
    requests: Arc<Mutex<Vec<Request>>>,
    task: JoinHandle<()>,
}
impl Fixture {
    async fn spawn(handler: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let handler = Arc::new(handler);
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (socket, _) = accepted.unwrap();
                        let captured = captured.clone();
                        let handler = handler.clone();
                        connections.spawn(async move {
                            let _ = respond(socket, captured, handler.as_ref()).await;
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }
    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
    fn config(&self) -> McpServerSettings {
        McpServerSettings {
            url: Some(self.url.clone()),
            startup_timeout_secs: 3,
            call_timeout_secs: 2,
            ..Default::default()
        }
    }
    async fn close(mut self) {
        self.task.abort();
        // Dropping the listener's JoinSet aborts and owns all connection tasks.
        let _ = (&mut self.task).await;
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn respond(
    mut socket: TcpStream,
    captured: Arc<Mutex<Vec<Request>>>,
    handler: &(dyn Fn(&Request) -> Reply + Send + Sync),
) -> io::Result<()> {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    let header_end = loop {
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
        if bytes.len() > 65536 {
            return Err(io::Error::other("fixture request headers too large"));
        }
    };
    let text = String::from_utf8_lossy(&bytes[..header_end]);
    let mut lines = text.lines();
    let mut first = lines.next().unwrap_or("").split_whitespace();
    let verb = first.next().unwrap_or("").to_owned();
    let path = first.next().unwrap_or("").to_owned();
    let headers: BTreeMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if length > 1024 * 1024 {
        return Err(io::Error::other("fixture request body too large"));
    }
    while bytes.len() < header_end + length {
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    let body = if length == 0 {
        Value::Null
    } else {
        serde_json::from_slice(&bytes[header_end..header_end + length]).map_err(io::Error::other)?
    };
    let request = Request {
        verb,
        path,
        headers,
        body,
        received: Instant::now(),
    };
    captured.lock().unwrap().push(request.clone());
    let reply = handler(&request);
    if let Some(barrier) = &reply.barrier {
        barrier.wait().await;
    }
    if reply.disconnect {
        return Ok(());
    }
    if !reply.delay.is_zero() {
        tokio::select! {
            _ = tokio::time::sleep(reply.delay) => {}
            _ = socket.read(&mut chunk) => return Ok(()),
        }
    }
    let mut head = format!(
        "HTTP/1.1 {} Fixture\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reply.content_type,
        reply.body.len()
    );
    for (name, value) in reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    socket.write_all(head.as_bytes()).await?;
    // Split writes exercise response streaming without relying on TCP boundaries.
    let midpoint = reply.body.len() / 2;
    socket.write_all(&reply.body.as_bytes()[..midpoint]).await?;
    if reply.truncate_body {
        return Ok(());
    }
    tokio::task::yield_now().await;
    socket.write_all(&reply.body.as_bytes()[midpoint..]).await?;
    socket.shutdown().await
}

fn definition(name: &str) -> Value {
    json!({"name":name, "description":"local fixture", "inputSchema":{"type":"object"}})
}
fn list(request: &Request) -> Reply {
    Reply::json(request, json!({"tools":[definition("echo")]}))
}
fn content(text: &str) -> Value {
    json!({"resultType":"complete", "content":[{"type":"text", "text":text}]})
}
fn modern(request: &Request) -> Reply {
    match request.method() {
        "tools/list" => list(request),
        "tools/call" => Reply::json(request, content("ok")),
        _ => Reply::status(405),
    }
}
fn legacy(request: &Request, version: &str) -> Reply {
    if request.verb == "DELETE" {
        return Reply::status(204);
    }
    match request.method() {
        "tools/list" if request.header("mcp-protocol-version") == Some(MODERN) => {
            Reply::status(400)
        }
        "initialize" => Reply::json(
            request,
            json!({
                "protocolVersion":version, "capabilities":{"tools":{}},
                "serverInfo":{"name":"local-fixture", "version":"1"}
            }),
        )
        .header("Mcp-Session-Id", "session-one"),
        "notifications/initialized" | "notifications/cancelled" => Reply::status(202),
        "tools/list" => list(request),
        "tools/call" => Reply::json(request, content("ok")),
        _ => Reply::status(405),
    }
}
async fn start(config: McpServerSettings, root: &Path) -> McpHost {
    McpHost::start(
        &McpSettings {
            servers: BTreeMap::from([("fixture".into(), config)]),
            ..Default::default()
        },
        root,
    )
    .await
}
fn tool(host: &McpHost, name: &str) -> Arc<dyn Tool> {
    host.tools()
        .into_iter()
        .find(|tool| tool.definition().name == format!("mcp__fixture__{name}"))
        .unwrap_or_else(|| panic!("missing {name}: {:?}", host.warnings()))
}
fn context() -> ToolContext {
    ToolContext {
        session_id: "http-test-session".into(),
        run_id: "http-test-run".into(),
        invocation_id: "http-test-invocation".into(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    }
}
async fn invoke(host: &McpHost, input: Value) -> ToolResult {
    tool(host, "echo").invoke(context(), input).await.unwrap()
}
async fn eventually(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(Instant::now() < deadline, "condition did not become true");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
fn calls(requests: &[Request]) -> Vec<&Request> {
    requests
        .iter()
        .filter(|request| request.method() == "tools/call")
        .collect()
}
fn assert_modern(request: &Request) {
    assert_eq!(request.verb, "POST");
    assert_eq!(request.path, "/mcp");
    assert_eq!(request.header("mcp-protocol-version"), Some(MODERN));
    assert_eq!(request.header("mcp-method"), Some(request.method()));
    assert_eq!(request.header("mcp-session-id"), None);
    let meta = &request.body["params"]["_meta"];
    assert_eq!(meta["io.modelcontextprotocol/protocolVersion"], MODERN);
    assert_eq!(
        meta["io.modelcontextprotocol/clientInfo"]["name"],
        "bubblepaw"
    );
    assert!(meta["io.modelcontextprotocol/clientInfo"]["version"].is_string());
    assert_eq!(
        meta["io.modelcontextprotocol/clientCapabilities"],
        json!({})
    );
    let accept = request.header("accept").unwrap();
    assert!(accept.contains("application/json") && accept.contains("text/event-stream"));
    if request.method() == "tools/call" {
        assert_eq!(
            request.header("mcp-name"),
            request.body["params"]["name"].as_str()
        );
    } else {
        assert_eq!(request.header("mcp-name"), None);
    }
}

#[tokio::test]
async fn modern_discovery_and_json_sse_calls_are_stateless_and_metadata_matches() {
    let fixture = Fixture::spawn(|request| {
        if request.body["params"]["arguments"]["sse"] == true {
            Reply::sse(request, content("streamed"))
        } else {
            modern(request).header("Mcp-Session-Id", "must-not-be-used")
        }
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let host = start(fixture.config(), root.path()).await;
    assert!(host.warnings().is_empty(), "{:?}", host.warnings());
    assert_eq!(host.tools().len(), 1);
    let definition = tool(&host, "echo").definition();
    assert!(!definition.read_only);
    assert_eq!(definition.approval, Approval::Confirm);
    assert_eq!(invoke(&host, json!({})).await.content, "ok");
    assert_eq!(invoke(&host, json!({"sse":true})).await.content, "streamed");
    host.shutdown(Duration::ZERO).await;
    let requests = fixture.requests();
    assert_eq!(
        requests.len(),
        3,
        "unexpected handshake, replay, GET, or DELETE"
    );
    assert_eq!(requests[0].method(), "tools/list");
    assert_eq!(calls(&requests).len(), 2);
    for request in &requests {
        assert_modern(request);
    }
    fixture.close().await;
}

#[tokio::test]
async fn recognized_modern_errors_never_trigger_legacy_initialization() {
    for code in [-32020, -32021, -32022] {
        let fixture = Fixture::spawn(move |request| {
            Reply::raw(
                400,
                "application/json",
                json!({
                    "jsonrpc":"2.0", "id":request.body["id"],
                    "error":{"code":code,"message":"recognized modern rejection"}
                })
                .to_string(),
            )
        })
        .await;
        let root = tempfile::tempdir().unwrap();
        let host = start(fixture.config(), root.path()).await;
        assert!(host.tools().is_empty());
        assert!(!host.warnings().is_empty());
        let expected = match code {
            -32020 => "header",
            -32021 => "capability",
            _ => "incompatible",
        };
        assert!(
            host.warnings()
                .iter()
                .any(|warning| warning.contains(expected)),
            "{:?}",
            host.warnings()
        );
        host.shutdown(Duration::ZERO).await;
        assert_eq!(
            fixture.requests().len(),
            1,
            "recognized error {code} downgraded"
        );
        assert_modern(&fixture.requests()[0]);
        fixture.close().await;
    }
}

#[tokio::test]
async fn legacy_negotiation_session_roundtrip_json_sse_and_delete() {
    for version in [LEGACY, "2025-06-18"] {
        let fixture = Fixture::spawn(move |request| {
            if request.method() == "tools/call"
                && request.body["params"]["arguments"]["sse"] == true
            {
                Reply::sse(request, content("legacy stream"))
            } else {
                legacy(request, version)
            }
        })
        .await;
        let root = tempfile::tempdir().unwrap();
        let host = start(fixture.config(), root.path()).await;
        assert!(host.warnings().is_empty(), "{:?}", host.warnings());
        assert_eq!(invoke(&host, json!({})).await.content, "ok");
        assert_eq!(
            invoke(&host, json!({"sse":true})).await.content,
            "legacy stream"
        );
        host.shutdown(Duration::from_secs(1)).await;
        let requests = fixture.requests();
        assert_eq!(requests.len(), 7);
        assert_modern(&requests[0]);
        assert_eq!(requests[1].method(), "initialize");
        assert_eq!(requests[1].body["params"]["protocolVersion"], LEGACY);
        assert_eq!(requests[1].header("mcp-protocol-version"), None);
        assert_eq!(requests[1].header("mcp-session-id"), None);
        assert_eq!(requests[2].method(), "notifications/initialized");
        assert_eq!(requests[3].method(), "tools/list");
        assert_eq!(requests[6].verb, "DELETE");
        for request in &requests[2..] {
            assert_eq!(request.header("mcp-session-id"), Some("session-one"));
            assert_eq!(request.header("mcp-protocol-version"), Some(version));
        }
        assert_eq!(calls(&requests).len(), 2);
        assert!(!requests.iter().any(|request| request.verb == "GET"));
        fixture.close().await;
    }
}

#[tokio::test]
async fn unrecognized_legacy_error_falls_back_but_incompatible_version_is_clear() {
    let fixture = Fixture::spawn(|request| {
        if request.header("mcp-protocol-version") == Some(MODERN) {
            Reply::raw(
                400,
                "application/json",
                json!({"jsonrpc":"2.0",
                    "id":request.body["id"], "error":{"code":-32042,"message":"legacy error"}
                })
                .to_string(),
            )
        } else {
            legacy(request, "2099-01-01")
        }
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let host = start(fixture.config(), root.path()).await;
    assert!(host.tools().is_empty());
    assert!(
        host.warnings()
            .iter()
            .any(|warning| warning.contains("incompatible")),
        "{:?}",
        host.warnings()
    );
    host.shutdown(Duration::ZERO).await;
    let requests = fixture.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method() == "initialize")
            .count(),
        1
    );
    assert!(
        !requests
            .iter()
            .any(|request| request.method() == "notifications/initialized")
    );
    fixture.close().await;
}

#[tokio::test]
async fn stale_session_404_reinitializes_once_without_stale_id_and_retries_rejected_call() {
    let generation = Arc::new(Mutex::new(0));
    let state = generation.clone();
    let fixture = Fixture::spawn(move |request| {
        if request.method() == "initialize" {
            let mut generation = state.lock().unwrap();
            *generation += 1;
            return legacy(request, LEGACY)
                .header("Mcp-Session-Id", &format!("session-{generation}"));
        }
        if request.method() == "tools/call" && request.header("mcp-session-id") == Some("session-1")
        {
            return Reply::status(404);
        }
        legacy(request, LEGACY)
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let host = start(fixture.config(), root.path()).await;
    let recovered = invoke(&host, json!({"stale":true})).await;
    assert!(!recovered.is_error, "{}", recovered.content);
    assert_eq!(recovered.content, "ok");
    assert_eq!(invoke(&host, json!({"fresh":true})).await.content, "ok");
    host.shutdown(Duration::from_secs(1)).await;
    let requests = fixture.requests();
    let initialize: Vec<_> = requests
        .iter()
        .filter(|request| request.method() == "initialize")
        .collect();
    assert_eq!(initialize.len(), 2);
    assert!(
        initialize
            .iter()
            .all(|request| request.header("mcp-session-id").is_none())
    );
    assert_eq!(calls(&requests).len(), 3);
    assert_eq!(
        calls(&requests)[0].header("mcp-session-id"),
        Some("session-1")
    );
    assert_eq!(
        calls(&requests)[1].header("mcp-session-id"),
        Some("session-2")
    );
    assert_eq!(
        calls(&requests)[2].header("mcp-session-id"),
        Some("session-2")
    );
    assert_eq!(
        calls(&requests)[0].body["params"]["arguments"],
        calls(&requests)[1].body["params"]["arguments"]
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method() == "tools/list"
                && request.header("mcp-protocol-version") != Some(MODERN))
            .count(),
        1
    );
    fixture.close().await;
}

#[tokio::test]
async fn disconnected_call_is_unknown_never_post_replayed_and_next_invocation_works() {
    for truncated_body in [false, true] {
        let fixture = Fixture::spawn(move |request| {
            if request.method() == "tools/call"
                && request.body["params"]["arguments"]["disconnect"] == true
            {
                if truncated_body {
                    Reply::json(request, content("accepted before socket loss")).truncated()
                } else {
                    Reply::disconnected()
                }
            } else {
                modern(request)
            }
        })
        .await;
        let root = tempfile::tempdir().unwrap();
        let host = start(fixture.config(), root.path()).await;
        let failed = invoke(&host, json!({"disconnect":true})).await;
        assert!(
            failed.is_error && failed.content.contains("unknown"),
            "{}",
            failed.content
        );
        assert_eq!(invoke(&host, json!({})).await.content, "ok");
        host.shutdown(Duration::ZERO).await;
        let requests = fixture.requests();
        assert_eq!(calls(&requests).len(), 2);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.body["params"]["arguments"]["disconnect"] == true)
                .count(),
            1
        );
        assert!(!requests.iter().any(|request| request.verb == "GET"));
        fixture.close().await;
    }
}

#[tokio::test]
async fn legacy_sse_priming_resumes_with_cursor_and_retry_without_replaying_post() {
    let pending = Arc::new(Mutex::new(None::<Request>));
    let state = pending.clone();
    let fixture = Fixture::spawn(move |request| {
        if request.verb == "GET" {
            let original = state.lock().unwrap().clone().unwrap();
            return Reply::sse(&original, content("resumed"));
        }
        if request.method() == "tools/call" {
            *state.lock().unwrap() = Some(request.clone());
            return Reply::raw(
                200,
                "text/event-stream",
                "retry: 120\nid: cursor-one\ndata:\n\n".into(),
            );
        }
        legacy(request, LEGACY)
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let host = start(fixture.config(), root.path()).await;
    assert_eq!(invoke(&host, json!({})).await.content, "resumed");
    host.shutdown(Duration::from_secs(1)).await;
    let requests = fixture.requests();
    assert_eq!(calls(&requests).len(), 1);
    let get: Vec<_> = requests
        .iter()
        .filter(|request| request.verb == "GET")
        .collect();
    assert_eq!(get.len(), 1);
    assert_eq!(get[0].header("last-event-id"), Some("cursor-one"));
    assert_eq!(get[0].header("mcp-session-id"), Some("session-one"));
    assert_eq!(get[0].header("mcp-protocol-version"), Some(LEGACY));
    assert!(
        get[0].received.duration_since(calls(&requests)[0].received) >= Duration::from_millis(100)
    );
    fixture.close().await;
}

#[tokio::test]
async fn interrupted_sse_without_cursor_or_with_get_405_is_unknown_without_legacy_sse_fallback() {
    for cursor in [false, true] {
        let fixture = Fixture::spawn(move |request| {
            if request.verb == "GET" {
                return Reply::status(405);
            }
            if request.method() == "tools/call" {
                return Reply::raw(
                    200,
                    "text/event-stream",
                    if cursor {
                        "id: unsupported-resume\nretry: 1\ndata:\n\n".into()
                    } else {
                        ": heartbeat without replay cursor\n\n".into()
                    },
                );
            }
            legacy(request, LEGACY)
        })
        .await;
        let root = tempfile::tempdir().unwrap();
        let host = start(fixture.config(), root.path()).await;
        let failed = invoke(&host, json!({})).await;
        assert!(
            failed.is_error && failed.content.contains("unknown"),
            "{}",
            failed.content
        );
        host.shutdown(Duration::from_secs(1)).await;
        let requests = fixture.requests();
        assert_eq!(calls(&requests).len(), 1);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.verb == "GET")
                .count(),
            usize::from(cursor)
        );
        assert!(requests.iter().all(|request| request.path == "/mcp"));
        fixture.close().await;
    }
}

#[tokio::test]
async fn concurrent_legacy_streams_resume_their_own_ids_and_results() {
    let pending = Arc::new(Mutex::new(BTreeMap::<String, Request>::new()));
    let state = pending.clone();
    let fixture = Fixture::spawn(move |request| {
        if request.verb == "GET" {
            let cursor = request.header("last-event-id").unwrap_or("");
            let original = state.lock().unwrap().get(cursor).cloned().unwrap();
            let text = original.body["params"]["arguments"]["text"]
                .as_str()
                .unwrap();
            return Reply::sse(&original, content(text));
        }
        if request.method() == "tools/call" {
            let cursor = format!(
                "cursor-{}",
                request.body["params"]["arguments"]["text"]
                    .as_str()
                    .unwrap()
            );
            state
                .lock()
                .unwrap()
                .insert(cursor.clone(), request.clone());
            return Reply::raw(
                200,
                "text/event-stream",
                format!("id: {cursor}\nretry: 1\ndata:\n\n"),
            );
        }
        legacy(request, LEGACY)
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let host = start(fixture.config(), root.path()).await;
    let (first, second) = tokio::join!(
        invoke(&host, json!({"text":"first"})),
        invoke(&host, json!({"text":"second"}))
    );
    assert_eq!(first.content, "first");
    assert_eq!(second.content, "second");
    host.shutdown(Duration::from_secs(1)).await;
    let requests = fixture.requests();
    assert_eq!(calls(&requests).len(), 2);
    let mut cursors: Vec<_> = requests
        .iter()
        .filter(|request| request.verb == "GET")
        .map(|request| request.header("last-event-id").unwrap())
        .collect();
    cursors.sort_unstable();
    assert_eq!(cursors, ["cursor-first", "cursor-second"]);
    assert_ne!(
        calls(&requests)[0].body["id"],
        calls(&requests)[1].body["id"]
    );
    fixture.close().await;
}

#[tokio::test]
async fn malformed_sse_is_bounded_unknown_and_never_replayed() {
    for oversized in [false, true] {
        let fixture = Fixture::spawn(move |request| {
            if request.method() == "tools/call" {
                let data = if oversized {
                    format!("data: {}\n\n", "x".repeat(9 * 1024 * 1024))
                } else {
                    "data: {not valid JSON}\n\n".into()
                };
                Reply::raw(200, "text/event-stream", data)
            } else {
                modern(request)
            }
        })
        .await;
        let root = tempfile::tempdir().unwrap();
        let host = start(fixture.config(), root.path()).await;
        let before = Instant::now();
        let failed = invoke(&host, json!({})).await;
        assert!(
            failed.is_error && failed.content.contains("unknown"),
            "{}",
            failed.content
        );
        assert!(
            failed.content.len() < 1024,
            "untrusted SSE echoed into diagnostic"
        );
        if oversized {
            assert!(
                failed.content.contains("limit"),
                "oversized frame reached timeout: {}",
                failed.content
            );
        }
        assert!(before.elapsed() < Duration::from_secs(3));
        host.shutdown(Duration::ZERO).await;
        assert_eq!(calls(&fixture.requests()).len(), 1);
        fixture.close().await;
    }
}

#[tokio::test]
async fn modern_timeout_and_cancellation_close_call_without_post_replay_or_notification() {
    let fixture = Fixture::spawn(|request| {
        if request.method() == "tools/call" && request.body["params"]["arguments"]["slow"] == true {
            Reply::sse(request, content("too late")).delayed()
        } else {
            modern(request)
        }
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let mut config = fixture.config();
    config.call_timeout_secs = 1;
    let host = start(config, root.path()).await;
    let failed = invoke(&host, json!({"slow":true})).await;
    assert!(failed.is_error && failed.content.contains("outcome unknown"));
    let echo = tool(&host, "echo");
    let context = context();
    let cancellation = context.cancellation.clone();
    let pending = tokio::spawn(async move { echo.invoke(context, json!({"slow":true})).await });
    eventually(|| calls(&fixture.requests()).len() == 2).await;
    cancellation.cancel();
    let cancelled = pending.await.unwrap();
    assert!(cancelled.is_err() || cancelled.as_ref().is_ok_and(|result| result.is_error));
    assert_eq!(invoke(&host, json!({})).await.content, "ok");
    host.shutdown(Duration::ZERO).await;
    let requests = fixture.requests();
    assert_eq!(calls(&requests).len(), 3);
    assert!(
        !requests
            .iter()
            .any(|request| request.method().starts_with("notifications/"))
    );
    assert!(requests.iter().all(|request| request.verb == "POST"));
    fixture.close().await;
}

#[tokio::test]
async fn annotated_nested_primitives_mirror_and_encode_headers_without_changing_arguments() {
    let fixture = Fixture::spawn(|request| {
        if request.method() == "tools/list" {
            let mut tool = definition("echo");
            tool["inputSchema"] = json!({"type":"object","properties":{
                "plain":{"type":"string","x-mcp-header":"Plain"},
                "unicode":{"type":"string","x-mcp-header":"Unicode"},
                "space":{"type":"string","x-mcp-header":"Space"},
                "sentinel":{"type":"string","x-mcp-header":"Sentinel"},
                "optional":{"type":"string","x-mcp-header":"Optional"},
                "nested":{"type":"object","properties":{
                    "count":{"type":"integer","x-mcp-header":"Count"},
                    "enabled":{"type":"boolean","x-mcp-header":"Enabled"}
                }}
            }});
            return Reply::json(request, json!({"tools":[tool]}));
        }
        modern(request)
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let host = start(fixture.config(), root.path()).await;
    assert!(host.warnings().is_empty(), "{:?}", host.warnings());
    let input = json!({"plain":"ordinary", "unicode":"猫", "space":" x ",
        "sentinel":"=?base64?eA==?=", "nested":{"count":42,"enabled":false}});
    assert_eq!(invoke(&host, input.clone()).await.content, "ok");
    host.shutdown(Duration::ZERO).await;
    let requests = fixture.requests();
    let call = calls(&requests)[0];
    assert_modern(call);
    assert_eq!(call.body["params"]["arguments"], input);
    assert_eq!(call.header("mcp-param-plain"), Some("ordinary"));
    assert_eq!(call.header("mcp-param-unicode"), Some("=?base64?54yr?="));
    assert_eq!(call.header("mcp-param-space"), Some("=?base64?IHgg?="));
    assert_eq!(
        call.header("mcp-param-sentinel"),
        Some("=?base64?PT9iYXNlNjQ/ZUE9PT89?=")
    );
    assert_eq!(call.header("mcp-param-count"), Some("42"));
    assert_eq!(call.header("mcp-param-enabled"), Some("false"));
    assert_eq!(call.header("mcp-param-optional"), None);
    fixture.close().await;
}

#[tokio::test]
async fn invalid_header_annotations_exclude_tools_with_bounded_warnings() {
    let schemas = [
        json!({"type":"object","properties":{"x":{"type":"number","x-mcp-header":"Float"}}}),
        json!({"type":"object","properties":{"x":{"type":"string","x-mcp-header":"Bad Header"}}}),
        json!({"type":"object","properties":{"x":{"type":"string","x-mcp-header":"Dup"},"y":{"type":"integer","x-mcp-header":"dup"}}}),
        json!({"type":"object","properties":{"x":{"type":"array","items":{"type":"string","x-mcp-header":"Item"}}}}),
        json!({"type":"object","allOf":[{"properties":{"x":{"type":"string","x-mcp-header":"Composition"}}}]}),
    ];
    let count = schemas.len();
    let fixture = Fixture::spawn(move |request| {
        if request.method() == "tools/list" {
            let mut tools = vec![definition("echo")];
            for (index, schema) in schemas.iter().enumerate() {
                let mut tool = definition(&format!("invalid_{index}"));
                tool["inputSchema"] = schema.clone();
                tools.push(tool);
            }
            Reply::json(request, json!({"tools":tools}))
        } else {
            modern(request)
        }
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let host = start(fixture.config(), root.path()).await;
    assert_eq!(
        host.tools().len(),
        1,
        "invalid annotation remained callable"
    );
    assert_eq!(host.warnings().len(), count, "{:?}", host.warnings());
    assert!(host.warnings().iter().all(|warning| warning.len() < 512));
    assert_eq!(invoke(&host, json!({})).await.content, "ok");
    host.shutdown(Duration::ZERO).await;
    fixture.close().await;
}

#[tokio::test]
async fn modern_closed_sse_never_attempts_get_even_with_event_id() {
    let fixture = Fixture::spawn(|request| {
        if request.method() == "tools/call" {
            Reply::raw(
                200,
                "text/event-stream",
                "id: modern-not-resumable\ndata:\n\n".into(),
            )
        } else {
            modern(request)
        }
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let host = start(fixture.config(), root.path()).await;
    let result = invoke(&host, json!({})).await;
    assert!(result.is_error && result.content.contains("unknown"));
    host.shutdown(Duration::ZERO).await;
    let requests = fixture.requests();
    assert_eq!(calls(&requests).len(), 1);
    assert!(requests.iter().all(|request| request.verb == "POST"));
    fixture.close().await;
}

#[tokio::test]
async fn modern_discovery_pagination_does_not_repeat_cached_first_page() {
    let fixture = Fixture::spawn(|request| {
        if request.method() == "tools/list" {
            if request.body["params"]["cursor"] == "second-page" {
                return Reply::sse(request, json!({"tools":[definition("second")]}));
            }
            return Reply::json(
                request,
                json!({"tools":[definition("echo")],"nextCursor":"second-page"}),
            );
        }
        modern(request)
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let host = start(fixture.config(), root.path()).await;
    assert!(host.warnings().is_empty(), "{:?}", host.warnings());
    assert_eq!(host.tools().len(), 2);
    host.shutdown(Duration::ZERO).await;
    let requests = fixture.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body["params"].get("cursor"), None);
    assert_eq!(requests[1].body["params"]["cursor"], "second-page");
    for request in &requests {
        assert_modern(request);
    }
    fixture.close().await;
}

#[tokio::test]
async fn unsupported_modern_input_required_is_not_followed_or_replayed() {
    let fixture = Fixture::spawn(|request| {
        if request.method() == "tools/call" {
            Reply::json(
                request,
                json!({"resultType":"input_required", "requestState":"opaque",
                "inputRequests":[{"type":"elicitation", "message":"Need input"}]}),
            )
        } else {
            modern(request)
        }
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let host = start(fixture.config(), root.path()).await;
    let result = invoke(&host, json!({})).await;
    assert!(result.is_error);
    assert!(
        result.content.contains("input") && result.content.contains("unsupported"),
        "{}",
        result.content
    );
    host.shutdown(Duration::ZERO).await;
    assert_eq!(calls(&fixture.requests()).len(), 1);
    assert_eq!(fixture.requests().len(), 2);
    fixture.close().await;
}

const SYNTHETIC_ENV: &str = "BUBBLEPAW_MCP_HTTP_TEST_AUTH_TOKEN";
const SYNTHETIC_VALUE: &str = "synthetic-http-auth-for-local-fixture-only";

#[tokio::test]
async fn env_header_values_are_resolved_and_redacted_in_isolated_process() {
    // The subprocess owns a synthetic env value. No unsafe process-wide env
    // mutations or races with the other tests, and no external server calls.
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .arg("--exact")
        .arg("env_header_child")
        .arg("--nocapture")
        .env(SYNTHETIC_ENV, SYNTHETIC_VALUE)
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "child failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains(SYNTHETIC_VALUE));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(SYNTHETIC_VALUE));
}

#[tokio::test]
async fn env_header_child() {
    if std::env::var(SYNTHETIC_ENV).ok().as_deref() != Some(SYNTHETIC_VALUE) {
        return;
    }
    let fixture = Fixture::spawn(|request| {
        if request.method() == "tools/call" {
            Reply::json(request, content(SYNTHETIC_VALUE))
        } else {
            modern(request)
        }
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let mut config = fixture.config();
    config
        .headers_from
        .insert("Authorization".into(), SYNTHETIC_ENV.into());
    let serialized = serde_json::to_string(&config).unwrap();
    assert!(serialized.contains(SYNTHETIC_ENV));
    assert!(!serialized.contains(SYNTHETIC_VALUE));
    assert!(!format!("{config:?}").contains(SYNTHETIC_VALUE));
    let host = start(config, root.path()).await;
    assert!(host.warnings().is_empty(), "{:?}", host.warnings());
    let result = invoke(&host, json!({})).await;
    assert!(result.content.contains("[redacted]"));
    assert!(!result.content.contains(SYNTHETIC_VALUE));
    host.shutdown(Duration::ZERO).await;
    assert!(
        fixture
            .requests()
            .iter()
            .all(|request| request.header("authorization") == Some(SYNTHETIC_VALUE))
    );
    fixture.close().await;
}

#[tokio::test]
async fn second_session_404_stops_after_one_reinitialization_and_one_retry() {
    let fixture = Fixture::spawn(|request| {
        if request.method() == "tools/call" {
            Reply::status(404)
        } else {
            legacy(request, LEGACY)
        }
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let host = start(fixture.config(), root.path()).await;
    let result = invoke(&host, json!({})).await;
    assert!(result.is_error);
    host.shutdown(Duration::from_secs(1)).await;
    let requests = fixture.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method() == "initialize")
            .count(),
        2
    );
    assert_eq!(calls(&requests).len(), 2);
    assert!(
        requests
            .iter()
            .filter(|request| request.method() == "initialize")
            .all(|request| request.header("mcp-session-id").is_none())
    );
    fixture.close().await;
}

#[tokio::test]
async fn modern_sse_discovery_and_unicode_tool_name_header_encoding() {
    let fixture = Fixture::spawn(|request| {
        if request.method() == "tools/list" {
            Reply::sse(request, json!({"tools":[definition("猫")]}))
        } else {
            modern(request)
        }
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let host = start(fixture.config(), root.path()).await;
    assert!(host.warnings().is_empty(), "{:?}", host.warnings());
    assert_eq!(host.tools().len(), 1);
    let result = host.tools()[0].invoke(context(), json!({})).await.unwrap();
    assert_eq!(result.content, "ok");
    host.shutdown(Duration::ZERO).await;
    let requests = fixture.requests();
    assert_eq!(requests.len(), 2);
    assert_modern(&requests[0]);
    let call = calls(&requests)[0];
    assert_eq!(call.header("mcp-name"), Some("=?base64?54yr?="));
    assert_eq!(call.body["params"]["name"], "猫");
    assert_eq!(call.header("mcp-method"), Some("tools/call"));
    assert_eq!(call.header("mcp-protocol-version"), Some(MODERN));
    assert_eq!(call.header("mcp-session-id"), None);
    fixture.close().await;
}

#[tokio::test]
async fn unreadable_modern_http_400_never_downgrades_to_initialize() {
    for oversized in [false, true] {
        let fixture = Fixture::spawn(move |request| {
            let body = if oversized {
                "x".repeat(9 * 1024 * 1024)
            } else {
                json!({"jsonrpc":"2.0", "id":request.body["id"],
                    "error":{"code":-32042,"message":"legacy rejection"}})
                .to_string()
            };
            let reply = Reply::raw(400, "application/json", body);
            if oversized { reply } else { reply.truncated() }
        })
        .await;
        let root = tempfile::tempdir().unwrap();
        let host = start(fixture.config(), root.path()).await;
        assert!(host.tools().is_empty());
        assert!(
            host.warnings()
                .iter()
                .any(|warning| warning.contains("unknown")),
            "{:?}",
            host.warnings()
        );
        host.shutdown(Duration::ZERO).await;
        let requests = fixture.requests();
        assert_eq!(
            requests.len(),
            1,
            "unreadable HTTP400 body caused a downgrade"
        );
        assert_modern(&requests[0]);
        fixture.close().await;
    }
}

#[tokio::test]
async fn complete_non_json_or_unrecognized_400_can_negotiate_legacy() {
    for body in [
        String::new(),
        "this legacy endpoint rejects stateless requests".into(),
        json!({"jsonrpc":"2.0", "error":{"code":-32042,"message":"old error"}}).to_string(),
    ] {
        let fixture = Fixture::spawn(move |request| {
            if request.header("mcp-protocol-version") == Some(MODERN) {
                Reply::raw(400, "application/json", body.clone())
            } else {
                legacy(request, LEGACY)
            }
        })
        .await;
        let root = tempfile::tempdir().unwrap();
        let host = start(fixture.config(), root.path()).await;
        assert!(host.warnings().is_empty(), "{:?}", host.warnings());
        assert_eq!(host.tools().len(), 1);
        host.shutdown(Duration::from_secs(1)).await;
        let requests = fixture.requests();
        assert_eq!(requests.len(), 5);
        assert_eq!(requests[0].method(), "tools/list");
        assert_eq!(requests[1].method(), "initialize");
        assert_eq!(requests[2].method(), "notifications/initialized");
        assert_eq!(requests[3].method(), "tools/list");
        assert_eq!(requests[4].verb, "DELETE");
        fixture.close().await;
    }
}

#[tokio::test]
async fn concurrent_stale_calls_do_not_use_session_from_failed_reinitialization() {
    let generations = Arc::new(Mutex::new(0));
    let state = generations.clone();
    let old_calls = Arc::new(tokio::sync::Barrier::new(2));
    let fixture = Fixture::spawn(move |request| {
        if request.method() == "initialize" {
            let mut generation = state.lock().unwrap();
            *generation += 1;
            return legacy(request, LEGACY)
                .header("Mcp-Session-Id", &format!("session-{}", *generation));
        }
        if request.method() == "notifications/initialized"
            && request.header("mcp-session-id") == Some("session-2")
        {
            return Reply::status(500);
        }
        if request.method() == "tools/call" && request.header("mcp-session-id") == Some("session-1")
        {
            // Both POSTs must arrive before either gets a rejection, so both
            // concurrent invocations deterministically hold the old snapshot.
            let mut reply = Reply::status(404);
            reply.barrier = Some(old_calls.clone());
            return reply;
        }
        legacy(request, LEGACY)
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let host = start(fixture.config(), root.path()).await;
    assert!(host.warnings().is_empty(), "{:?}", host.warnings());
    let (first, second) = tokio::join!(
        invoke(&host, json!({"text":"first"})),
        invoke(&host, json!({"text":"second"}))
    );
    assert_ne!(
        first.is_error, second.is_error,
        "exactly the failed reinitialization must fail its call"
    );
    assert_eq!(
        if first.is_error {
            &second.content
        } else {
            &first.content
        },
        "ok"
    );
    host.shutdown(Duration::from_secs(1)).await;
    let requests = fixture.requests();
    let initialize: Vec<_> = requests
        .iter()
        .filter(|request| request.method() == "initialize")
        .collect();
    assert_eq!(initialize.len(), 3);
    assert!(
        initialize
            .iter()
            .all(|request| request.header("mcp-session-id").is_none())
    );
    let calls = calls(&requests);
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls
            .iter()
            .filter(|request| request.header("mcp-session-id") == Some("session-1"))
            .count(),
        2
    );
    assert!(
        !calls
            .iter()
            .any(|request| request.header("mcp-session-id") == Some("session-2"))
    );
    assert_eq!(
        calls
            .iter()
            .filter(|request| request.header("mcp-session-id") == Some("session-3"))
            .count(),
        1
    );
    let initialized = requests
        .iter()
        .position(|request| {
            request.method() == "notifications/initialized"
                && request.header("mcp-session-id") == Some("session-3")
        })
        .unwrap();
    let retried = requests
        .iter()
        .position(|request| {
            request.method() == "tools/call"
                && request.header("mcp-session-id") == Some("session-3")
        })
        .unwrap();
    assert!(
        initialized < retried,
        "retry preceded successful initialization"
    );
    fixture.close().await;
}

#[tokio::test]
async fn resumed_initialize_result_assigns_session_before_legacy_requests() {
    for session_on_post in [false, true] {
        let initialize = Arc::new(Mutex::new(None::<Request>));
        let state = initialize.clone();
        let session = if session_on_post {
            "session-from-post"
        } else {
            "session-from-get"
        };
        let fixture = Fixture::spawn(move |request| {
            if request.method() == "initialize" {
                *state.lock().unwrap() = Some(request.clone());
                let reply = Reply::raw(
                    200,
                    "text/event-stream",
                    "id: cursor-init\nretry: 1\ndata:\n\n".into(),
                );
                return if session_on_post {
                    reply.header("Mcp-Session-Id", session)
                } else {
                    reply
                };
            }
            if request.verb == "GET" {
                let original = state.lock().unwrap().clone().unwrap();
                return Reply::sse(
                    &original,
                    json!({
                        "protocolVersion":"2025-06-18", "capabilities":{"tools":{}},
                        "serverInfo":{"name":"local-fixture","version":"1"}
                    }),
                )
                .header("Mcp-Session-Id", session);
            }
            legacy(request, "2025-06-18")
        })
        .await;
        let root = tempfile::tempdir().unwrap();
        let host = start(fixture.config(), root.path()).await;
        assert!(host.warnings().is_empty(), "{:?}", host.warnings());
        assert_eq!(invoke(&host, json!({})).await.content, "ok");
        host.shutdown(Duration::from_secs(1)).await;
        let requests = fixture.requests();
        assert_eq!(
            requests.len(),
            7,
            "unexpected initialization replay or missing session close"
        );
        assert_modern(&requests[0]);
        assert_eq!(requests[1].method(), "initialize");
        assert_eq!(requests[1].header("mcp-session-id"), None);
        assert_eq!(requests[1].body["params"]["protocolVersion"], LEGACY);
        assert_eq!(requests[1].header("mcp-protocol-version"), None);
        assert_eq!(requests[2].verb, "GET");
        assert_eq!(requests[2].header("last-event-id"), Some("cursor-init"));
        assert_eq!(requests[2].header("mcp-protocol-version"), None);
        assert_eq!(
            requests[2].header("mcp-session-id"),
            if session_on_post { Some(session) } else { None }
        );
        assert_eq!(requests[3].method(), "notifications/initialized");
        assert_eq!(requests[4].method(), "tools/list");
        assert_eq!(requests[5].method(), "tools/call");
        assert_eq!(requests[6].verb, "DELETE");
        for request in &requests[3..] {
            assert_eq!(request.header("mcp-session-id"), Some(session));
            assert_eq!(request.header("mcp-protocol-version"), Some("2025-06-18"));
        }
        assert_eq!(calls(&requests).len(), 1);
        fixture.close().await;
    }
}

#[tokio::test]
async fn strict_older_server_negotiates_header_free_initialize_before_versioned_requests() {
    let fixture = Fixture::spawn(|request| {
        if request
            .header("mcp-protocol-version")
            .is_some_and(|version| version != "2025-06-18")
        {
            // This server understands only June2025 headers. The modern probe
            // gets400, but initialization remains available without a header.
            return Reply::status(400);
        }
        if request.method() == "initialize" {
            return legacy(request, "2025-06-18");
        }
        if request.header("mcp-protocol-version") != Some("2025-06-18") {
            return Reply::status(412);
        }
        legacy(request, "2025-06-18")
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let host = start(fixture.config(), root.path()).await;
    assert!(host.warnings().is_empty(), "{:?}", host.warnings());
    assert_eq!(invoke(&host, json!({})).await.content, "ok");
    host.shutdown(Duration::from_secs(1)).await;
    let requests = fixture.requests();
    assert_eq!(requests.len(), 6);
    assert_modern(&requests[0]);
    assert_eq!(requests[1].method(), "initialize");
    assert_eq!(requests[1].body["params"]["protocolVersion"], LEGACY);
    assert_eq!(requests[1].header("mcp-protocol-version"), None);
    assert_eq!(requests[1].header("mcp-session-id"), None);
    assert_eq!(requests[2].method(), "notifications/initialized");
    assert_eq!(requests[3].method(), "tools/list");
    assert_eq!(requests[4].method(), "tools/call");
    assert_eq!(requests[5].verb, "DELETE");
    for request in &requests[2..] {
        assert_eq!(request.header("mcp-protocol-version"), Some("2025-06-18"));
        assert_eq!(request.header("mcp-session-id"), Some("session-one"));
    }
    assert_eq!(calls(&requests).len(), 1);
    fixture.close().await;
}

#[tokio::test]
async fn explicit_supported_version_selects_known_legacy_but_unknown_offer_is_incompatible() {
    for supported in ["2025-06-18", "2099-01-01"] {
        let fixture = Fixture::spawn(move |request| {
            if request.header("mcp-protocol-version") == Some(MODERN) {
                return Reply::raw(
                    400,
                    "application/json",
                    json!({
                        "jsonrpc":"2.0", "id":request.body["id"],
                        "error":{"code":-32022,"message":"Unsupported protocol version",
                            "data":{"supported":[supported]}}
                    })
                    .to_string(),
                );
            }
            legacy(request, "2025-06-18")
        })
        .await;
        let root = tempfile::tempdir().unwrap();
        let host = start(fixture.config(), root.path()).await;
        if supported == "2025-06-18" {
            assert!(host.warnings().is_empty(), "{:?}", host.warnings());
            assert_eq!(invoke(&host, json!({})).await.content, "ok");
            host.shutdown(Duration::from_secs(1)).await;
            let requests = fixture.requests();
            assert_eq!(requests.len(), 6);
            assert_modern(&requests[0]);
            assert_eq!(requests[1].method(), "initialize");
            assert_eq!(requests[1].body["params"]["protocolVersion"], supported);
            assert_eq!(requests[1].header("mcp-protocol-version"), None);
            assert_eq!(requests[1].header("mcp-session-id"), None);
            assert_eq!(requests[2].method(), "notifications/initialized");
            assert_eq!(requests[3].method(), "tools/list");
            assert_eq!(requests[4].method(), "tools/call");
            assert_eq!(requests[5].verb, "DELETE");
            for request in &requests[2..] {
                assert_eq!(request.header("mcp-protocol-version"), Some(supported));
                assert_eq!(request.header("mcp-session-id"), Some("session-one"));
            }
            assert_eq!(calls(&requests).len(), 1);
        } else {
            assert!(host.tools().is_empty());
            assert!(
                host.warnings()
                    .iter()
                    .any(|warning| warning.contains("incompatible")),
                "{:?}",
                host.warnings()
            );
            host.shutdown(Duration::ZERO).await;
            let requests = fixture.requests();
            assert_eq!(
                requests.len(),
                1,
                "unrecognized advertised version caused initialization"
            );
            assert_modern(&requests[0]);
        }
        fixture.close().await;
    }
}

#[tokio::test]
async fn response_stream_tool_list_notifications_refresh_modern_and_legacy_inventories() {
    for use_legacy in [false, true] {
        let listed = Arc::new(Mutex::new(vec![definition("echo"), definition("removed")]));
        let published = listed.clone();
        let fixture = Fixture::spawn(move |request| {
            if use_legacy && (request.method() != "tools/list" || request.header("mcp-protocol-version") == Some(MODERN)) && request.method() != "tools/call" {
                return legacy(request, LEGACY);
            }
            match request.method() {
                "tools/list" => Reply::json(request, json!({"tools":*published.lock().unwrap()})),
                "tools/call" if request.body["params"]["arguments"]["notify"] == true => {
                    Reply::raw(200, "text/event-stream", format!(
                        "data: {}\n\ndata: {}\n\n",
                        json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"}),
                        json!({"jsonrpc":"2.0","id":request.body["id"],"result":content("notified")}),
                    ))
                }
                "tools/call" => Reply::json(request, content("current")),
                _ => legacy(request, LEGACY),
            }
        }).await;
        let root = tempfile::tempdir().unwrap();
        let host = start(fixture.config(), root.path()).await;
        let old_echo = tool(&host, "echo");
        let removed = tool(&host, "removed");
        *listed.lock().unwrap() = vec![
            json!({"name":"echo","description":"updated description","inputSchema":{"type":"object"}}),
            definition("added"),
            json!({"name":"invalid","inputSchema":{"type":17}}),
        ];
        assert_eq!(
            old_echo
                .invoke(context(), json!({"notify":true}))
                .await
                .unwrap()
                .content,
            "notified"
        );
        let diff = host.refresh_tools().await;
        assert_eq!(diff.added, ["mcp__fixture__added"]);
        assert_eq!(diff.removed, ["mcp__fixture__removed"]);
        assert_eq!(diff.changed, ["mcp__fixture__echo"]);
        assert!(
            diff.warnings
                .iter()
                .any(|w| w.contains("invalid input schema"))
        );
        assert!(
            removed
                .invoke(context(), json!({}))
                .await
                .unwrap()
                .content
                .contains("removed")
        );
        assert!(
            old_echo
                .invoke(context(), json!({}))
                .await
                .unwrap()
                .content
                .contains("definition changed")
        );
        assert_eq!(invoke(&host, json!({})).await.content, "current");
        assert!(host.refresh_tools().await.is_empty());
        let requests = fixture.requests();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.method() == "initialize")
                .count(),
            usize::from(use_legacy)
        );
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.method() == "tools/list")
                .count(),
            if use_legacy { 3 } else { 2 }
        );
        host.shutdown(Duration::from_millis(100)).await;
        fixture.close().await;
    }
}

#[tokio::test]
async fn legacy_generation_retires_session_only_after_its_in_flight_call_completes() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let generation = AtomicUsize::new(0);
    let fixture = Fixture::spawn(move |request| {
        if request.method() == "initialize" {
            let next = generation.fetch_add(1, Ordering::SeqCst) + 1;
            return legacy(request, LEGACY).header("Mcp-Session-Id", &format!("generation-{next}"));
        }
        if request.method() == "tools/call" {
            let mut reply =
                Reply::json(request, content(request.header("mcp-session-id").unwrap()));
            if request.body["params"]["arguments"]["slow"] == true {
                reply.delay = Duration::from_millis(400);
            }
            return reply;
        }
        legacy(request, LEGACY)
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let config = McpSettings {
        servers: BTreeMap::from([("fixture".into(), fixture.config())]),
        ..Default::default()
    };
    let host = Arc::new(McpHost::start(&config, root.path()).await);
    let retained = tool(&host, "echo");
    let slow = retained.clone();
    let pending =
        tokio::spawn(async move { slow.invoke(context(), json!({"slow":true})).await.unwrap() });
    eventually(|| {
        fixture
            .requests()
            .iter()
            .any(|r| r.method() == "tools/call")
    })
    .await;
    host.reload(&config, Some("fixture")).await.unwrap();
    assert_eq!(
        retained.invoke(context(), json!({})).await.unwrap().content,
        "generation-2"
    );
    assert!(
        !fixture
            .requests()
            .iter()
            .any(|r| r.verb == "DELETE" && r.header("mcp-session-id") == Some("generation-1"))
    );
    assert_eq!(pending.await.unwrap().content, "generation-1");
    eventually(|| {
        fixture
            .requests()
            .iter()
            .any(|r| r.verb == "DELETE" && r.header("mcp-session-id") == Some("generation-1"))
    })
    .await;
    assert_eq!(calls(&fixture.requests()).len(), 2);
    host.shutdown(Duration::from_millis(100)).await;
    fixture.close().await;
}

#[tokio::test]
async fn cancelling_tool_list_refresh_preserves_inventory_and_retries_notification() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let lists = AtomicUsize::new(0);
    let fixture = Fixture::spawn(move |request| match request.method() {
        "tools/list" => {
            let index = lists.fetch_add(1, Ordering::SeqCst);
            let mut reply = if index == 0 {
                list(request)
            } else {
                Reply::json(
                    request,
                    json!({"tools":[definition("echo"),definition("added")]}),
                )
            };
            if index == 1 {
                reply.delay = Duration::from_secs(10);
            }
            reply
        }
        "tools/call" => Reply::raw(
            200,
            "text/event-stream",
            format!(
                "data: {}\n\ndata: {}\n\n",
                json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"}),
                json!({"jsonrpc":"2.0","id":request.body["id"],"result":content("notified")}),
            ),
        ),
        _ => Reply::status(405),
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let host = Arc::new(start(fixture.config(), root.path()).await);
    invoke(&host, json!({})).await;
    let refreshing = host.clone();
    let pending = tokio::spawn(async move { refreshing.refresh_tools().await });
    eventually(|| {
        fixture
            .requests()
            .iter()
            .filter(|r| r.method() == "tools/list")
            .count()
            == 2
    })
    .await;
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    assert_eq!(host.tools().len(), 1);
    let diff = host.refresh_tools().await;
    assert_eq!(diff.added, ["mcp__fixture__added"]);
    assert_eq!(host.tools().len(), 2);
    assert!(host.refresh_tools().await.is_empty());
    host.shutdown(Duration::ZERO).await;
    fixture.close().await;
}

const CLIENT_CREDENTIAL: &str = "BUBBLEPAW_MCP_CLIENT_TEST_TOKEN";

#[tokio::test]
async fn client_env_headers_differ_per_host_and_never_fall_back_to_host_env() {
    let fixture = Fixture::spawn(|request| {
        if request.method() == "tools/call" {
            Reply::json(request, content("ok"))
        } else {
            modern(request)
        }
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let mut config = fixture.config();
    config
        .headers_from
        .insert("Authorization".into(), CLIENT_CREDENTIAL.into());
    let settings = McpSettings {
        servers: BTreeMap::from([("fixture".into(), config.clone())]),
        ..Default::default()
    };
    let first_value = "Bearer client-first-value";
    let second_value = "Bearer client-second-value";

    let client = McpClientEnv::new([(CLIENT_CREDENTIAL, first_value)]);
    assert!(!format!("{client:?}").contains("client-first-value"));
    let first = McpHost::start_with_client_env(&settings, root.path(), client).await;
    assert!(first.warnings().is_empty(), "{:?}", first.warnings());
    let result = invoke(&first, json!({})).await;
    assert!(!result.is_error);
    assert_eq!(result.content, "ok");
    first.shutdown(Duration::ZERO).await;
    let before_second = fixture.requests();
    assert!(!before_second.is_empty());
    assert!(
        before_second
            .iter()
            .all(|request| request.header("authorization") == Some(first_value))
    );

    let second = McpHost::start_with_client_env(
        &settings,
        root.path(),
        McpClientEnv::new([(CLIENT_CREDENTIAL, second_value)]),
    )
    .await;
    assert!(second.warnings().is_empty(), "{:?}", second.warnings());
    assert!(!invoke(&second, json!({})).await.is_error);
    second.shutdown(Duration::ZERO).await;
    let all = fixture.requests();
    let second_requests = &all[before_second.len()..];
    assert!(!second_requests.is_empty());
    assert!(
        second_requests
            .iter()
            .all(|request| request.header("authorization") == Some(second_value))
    );

    // The host env may hold the same name, but a snapshot miss must not read it.
    // PATH is always set in the test process, so only the snapshot can cause this error.
    let mut fallback = config;
    fallback.headers_from = BTreeMap::from([("Authorization".into(), "PATH".into())]);
    let missing = McpHost::start_with_client_env(
        &McpSettings {
            header_key_envs: vec!["PATH".into()],
            servers: BTreeMap::from([("fixture".into(), fallback)]),
        },
        root.path(),
        McpClientEnv::new([("UNRELATED_CLIENT_VALUE", "unused")]),
    )
    .await;
    assert!(missing.tools().is_empty());
    let warning = missing.warnings().join("\n");
    assert!(
        warning.contains("required MCP HTTP credential environment variable is missing"),
        "{warning}"
    );
    missing.shutdown(Duration::ZERO).await;
    assert_eq!(fixture.requests().len(), all.len());
    fixture.close().await;
}

#[tokio::test]
async fn client_snapshot_survives_forced_reload() {
    let fixture = Fixture::spawn(|request| {
        if request.method() == "tools/call" {
            Reply::json(request, content("ok"))
        } else {
            modern(request)
        }
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let mut config = fixture.config();
    config
        .headers_from
        .insert("Authorization".into(), CLIENT_CREDENTIAL.into());
    let settings = McpSettings {
        servers: BTreeMap::from([("fixture".into(), config)]),
        ..Default::default()
    };
    let value = "Bearer client-reload-value";
    let host = McpHost::start_with_client_env(
        &settings,
        root.path(),
        McpClientEnv::new([(CLIENT_CREDENTIAL, value)]),
    )
    .await;
    assert!(host.warnings().is_empty(), "{:?}", host.warnings());
    let before = fixture.requests().len();
    // Forcing a reload restarts the server with the same settings; the host keeps its snapshot.
    let diff = host.reload(&settings, Some("fixture")).await.unwrap();
    assert_eq!(diff.restarted, ["fixture"]);
    assert!(!invoke(&host, json!({})).await.is_error);
    host.shutdown(Duration::ZERO).await;
    let requests = fixture.requests();
    let after = &requests[before..];
    assert!(!after.is_empty());
    assert!(
        after
            .iter()
            .all(|request| request.header("authorization") == Some(value))
    );
    fixture.close().await;
}
