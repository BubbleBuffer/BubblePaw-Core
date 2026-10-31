//! MCP authorization (OAuth 2.1) for HTTP servers: protected-resource and
//! authorization-server discovery, dynamic client registration, authorization
//! code with PKCE, and refresh. The host never opens a browser; it returns the
//! authorization URL and receives the code on a loopback redirect or as a
//! pasted callback URL. Tokens live only in a user-only file and in memory.
//! Remote bodies never reach diagnostics: errors name the step and, at most,
//! a sanitized OAuth error code.
use anyhow::{Result, anyhow, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{
    Client, Response, Url,
    header::{ACCEPT, CONTENT_TYPE, HeaderValue},
};
use serde_json::Value;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod discovery;
mod flow;
mod loopback;
mod session;
pub(crate) mod store;

pub(crate) use discovery::Challenge;
pub(crate) use session::{Changed, ServerAuth};

const BODY_LIMIT: usize = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// No redirects, retries or inherited proxies, like the MCP transport.
pub(crate) fn http_client() -> Result<Client> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(10))
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|_| anyhow!("cannot create MCP OAuth HTTP client"))
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// 32 random bytes, base64url: PKCE verifiers and `state` values.
pub(crate) fn random_token() -> String {
    use rand_core::RngCore;
    let mut bytes = [0u8; 32];
    rand_core::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Endpoints follow the server URL rule: HTTPS, or plain HTTP only on this
/// machine. Credentials and fragments are refused.
pub(crate) fn endpoint(text: &str, what: &str) -> Result<Url> {
    let url = Url::parse(text).map_err(|_| anyhow!("MCP OAuth {what} is not a valid URL"))?;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host.ends_with(".localhost")
            || host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    });
    ensure!(
        (url.scheme() == "https" || (url.scheme() == "http" && loopback))
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "MCP OAuth {what} must use https (or http on this machine) without credentials"
    );
    Ok(url)
}

/// An OAuth error code is a short ASCII token; anything else is dropped so a
/// remote body never reaches a diagnostic.
pub(crate) fn error_code(body: &Value) -> Option<String> {
    body["error"]
        .as_str()
        .filter(|code| {
            (1..=64).contains(&code.len())
                && code
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        })
        .map(str::to_owned)
}

pub(crate) async fn read_json(response: Response) -> Option<Value> {
    if response
        .content_length()
        .is_some_and(|length| length > BODY_LIMIT as u64)
    {
        return None;
    }
    let mut response = response;
    let mut bytes = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        if bytes.len() + chunk.len() > BODY_LIMIT {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).ok()
}

pub(crate) async fn get_json(client: &Client, url: &Url) -> Result<Option<Value>> {
    let response = client
        .get(url.clone())
        .header(ACCEPT, HeaderValue::from_static("application/json"))
        .send()
        .await
        .map_err(|_| anyhow!("MCP OAuth metadata request failed"))?;
    if !response.status().is_success() {
        return Ok(None);
    }
    Ok(read_json(response).await.filter(Value::is_object))
}

/// `application/x-www-form-urlencoded` with `url`'s own encoder.
pub(crate) fn form(pairs: &[(&str, &str)]) -> String {
    let mut url = Url::parse("http://localhost/").expect("static URL");
    url.query_pairs_mut().extend_pairs(pairs);
    url.query().unwrap_or_default().to_owned()
}

pub(crate) async fn post_form(
    client: &Client,
    url: &Url,
    pairs: &[(&str, &str)],
) -> Result<(u16, Option<Value>)> {
    let response = client
        .post(url.clone())
        .header(ACCEPT, HeaderValue::from_static("application/json"))
        .header(
            CONTENT_TYPE,
            HeaderValue::from_static("application/x-www-form-urlencoded"),
        )
        .body(form(pairs))
        .send()
        .await
        .map_err(|_| anyhow!("MCP OAuth token request failed"))?;
    let status = response.status().as_u16();
    Ok((status, read_json(response).await))
}
