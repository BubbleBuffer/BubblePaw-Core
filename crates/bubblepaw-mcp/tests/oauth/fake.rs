//! A local fake of an OAuth-protected MCP server and its authorization server:
//! protected-resource and authorization-server metadata, dynamic client
//! registration, a token endpoint that verifies PKCE, refresh with rotation,
//! and a modern Streamable HTTP endpoint that requires the current token.
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::{JoinHandle, JoinSet},
};

#[derive(Default)]
pub struct State {
    pub registrations: Vec<Value>,
    /// code -> (PKCE challenge, redirect_uri)
    pub codes: BTreeMap<String, (String, String)>,
    pub access: Option<String>,
    pub refresh: Option<String>,
    pub issued: Vec<String>,
    pub refresh_grants: usize,
    pub refuse_refresh: bool,
    pub pkce_failures: usize,
    pub mcp_authorizations: Vec<Option<String>>,
    pub resources: Vec<String>,
    serial: usize,
}

impl State {
    fn issue(&mut self) -> Value {
        self.serial += 1;
        let access = format!("AT-fake-secret-{}", self.serial);
        let refresh = format!("RT-fake-secret-{}", self.serial);
        self.issued.extend([access.clone(), refresh.clone()]);
        self.access = Some(access.clone());
        self.refresh = Some(refresh.clone());
        json!({"access_token":access, "token_type":"Bearer", "expires_in":3600, "refresh_token":refresh})
    }
}

pub struct Fake {
    pub base: String,
    pub state: Arc<Mutex<State>>,
    task: JoinHandle<()>,
}

impl Fake {
    pub async fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State::default()));
        let (shared, origin) = (state.clone(), base.clone());
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (socket, _) = accepted.unwrap();
                        let (shared, origin) = (shared.clone(), origin.clone());
                        connections.spawn(async move {
                            let _ = serve(socket, &origin, &shared).await;
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Self { base, state, task }
    }

    pub fn url(&self) -> String {
        format!("{}/mcp", self.base)
    }

    /// The user approves the sign-in: the authorization server mints a code
    /// bound to the request's PKCE challenge and redirect.
    pub fn approve(&self, authorization_url: &str, code: &str) -> (String, String) {
        let url = reqwest::Url::parse(authorization_url).unwrap();
        let get = |name: &str| {
            url.query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
                .unwrap_or_else(|| panic!("authorization URL lacks {name}"))
        };
        assert_eq!(url.path(), "/authorize");
        assert_eq!(get("response_type"), "code");
        assert_eq!(get("code_challenge_method"), "S256");
        assert_eq!(get("resource"), self.url());
        assert_eq!(get("scope"), "notes.read");
        assert_eq!(get("client_id"), "client-1");
        let redirect = get("redirect_uri");
        self.state
            .lock()
            .unwrap()
            .codes
            .insert(code.into(), (get("code_challenge"), redirect.clone()));
        (redirect, get("state"))
    }

    pub fn issued(&self) -> Vec<String> {
        self.state.lock().unwrap().issued.clone()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn form(body: &str) -> BTreeMap<String, String> {
    reqwest::Url::parse(&format!("http://localhost/?{body}"))
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

fn route(
    verb: &str,
    path: &str,
    headers: &BTreeMap<String, String>,
    body: &str,
    base: &str,
    state: &Mutex<State>,
) -> (u16, Vec<(String, String)>, String) {
    let ok = |value: Value| (200, Vec::new(), value.to_string());
    let mut state = state.lock().unwrap();
    match (verb, path) {
        ("GET", "/.well-known/oauth-protected-resource/mcp") => ok(json!({
            "resource": format!("{base}/mcp"),
            "authorization_servers": [base],
            "scopes_supported": ["notes.read"],
        })),
        ("GET", "/.well-known/oauth-authorization-server") => ok(json!({
            "issuer": base,
            "authorization_endpoint": format!("{base}/authorize"),
            "token_endpoint": format!("{base}/token"),
            "registration_endpoint": format!("{base}/register"),
            "code_challenge_methods_supported": ["S256"],
        })),
        ("POST", "/register") => {
            let request: Value = serde_json::from_str(body).unwrap_or_default();
            state.registrations.push(request);
            (201, Vec::new(), json!({"client_id":"client-1"}).to_string())
        }
        ("POST", "/token") => {
            let form = form(body);
            let field = |name: &str| form.get(name).cloned().unwrap_or_default();
            state.resources.push(field("resource"));
            let refused = |code: &str| (400, Vec::new(), json!({"error":code}).to_string());
            if field("client_id") != "client-1" {
                return refused("invalid_client");
            }
            match field("grant_type").as_str() {
                "authorization_code" => {
                    let Some((challenge, redirect)) = state.codes.remove(&field("code")) else {
                        return refused("invalid_grant");
                    };
                    let computed =
                        URL_SAFE_NO_PAD.encode(Sha256::digest(field("code_verifier").as_bytes()));
                    if computed != challenge || redirect != field("redirect_uri") {
                        state.pkce_failures += 1;
                        return refused("invalid_grant");
                    }
                    ok(state.issue())
                }
                "refresh_token" => {
                    state.refresh_grants += 1;
                    if state.refuse_refresh
                        || state.refresh.as_deref() != Some(&field("refresh_token"))
                    {
                        return refused("invalid_grant");
                    }
                    ok(state.issue())
                }
                _ => refused("unsupported_grant_type"),
            }
        }
        ("POST", "/mcp") => {
            let authorization = headers.get("authorization").cloned();
            state.mcp_authorizations.push(authorization.clone());
            let expected = state
                .access
                .as_ref()
                .map(|access| format!("Bearer {access}"));
            if authorization.is_none() || authorization != expected {
                let challenge = format!(
                    "Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource/mcp\", scope=\"notes.read\""
                );
                return (
                    401,
                    vec![("WWW-Authenticate".into(), challenge)],
                    String::new(),
                );
            }
            let request: Value = serde_json::from_str(body).unwrap_or_default();
            let id = request["id"].clone();
            let result = match request["method"].as_str() {
                Some("tools/list") => json!({"tools":[
                    {"name":"search", "description":"fake", "inputSchema":{"type":"object"}},
                    {"name":"delete_all", "description":"fake", "inputSchema":{"type":"object"}},
                ]}),
                // Echo the token so redaction is visible in the result.
                Some("tools/call") => json!({"resultType":"complete", "content":[
                    {"type":"text", "text": format!("token was {}", state.access.clone().unwrap())}
                ]}),
                _ => return (405, Vec::new(), String::new()),
            };
            ok(json!({"jsonrpc":"2.0", "id":id, "result":result}))
        }
        _ => (404, Vec::new(), String::new()),
    }
}

async fn serve(mut socket: TcpStream, base: &str, state: &Mutex<State>) -> io::Result<()> {
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
    };
    let text = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
    let mut lines = text.lines();
    let mut first = lines.next().unwrap_or("").split_whitespace();
    let verb = first.next().unwrap_or("").to_owned();
    let target = first.next().unwrap_or("").to_owned();
    let path = target.split('?').next().unwrap_or("").to_owned();
    let headers: BTreeMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    while bytes.len() < header_end + length {
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    let body = String::from_utf8_lossy(&bytes[header_end..header_end + length]).into_owned();
    let (status, extra, body) = route(&verb, &path, &headers, &body, base, state);
    let mut head = format!(
        "HTTP/1.1 {status} Fake\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    socket.write_all(head.as_bytes()).await?;
    socket.write_all(body.as_bytes()).await?;
    socket.shutdown().await
}
