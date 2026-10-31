//! RFC 7591 registration, authorization code with PKCE (S256), and refresh.
use super::{
    discovery::Metadata, endpoint, error_code, now, post_form, random_token, read_json,
    store::Tokens,
};
use anyhow::{Result, anyhow, bail, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{
    Client, Url,
    header::{ACCEPT, CONTENT_TYPE, HeaderValue},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// One sign-in attempt. Dropped once it completes, fails or is cancelled.
pub(crate) struct Pending {
    pub server_url: String,
    pub metadata: Metadata,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub redirect_uri: String,
    pub state: String,
    verifier: String,
}

impl Pending {
    pub(crate) async fn new(
        client: &Client,
        server_url: String,
        metadata: Metadata,
        redirect_uri: String,
    ) -> Result<Self> {
        let (client_id, client_secret) = register(client, &metadata, &redirect_uri).await?;
        Ok(Self {
            server_url,
            metadata,
            client_id,
            client_secret,
            redirect_uri,
            state: random_token(),
            verifier: random_token(),
        })
    }

    pub(crate) fn authorization_url(&self) -> String {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(self.verifier.as_bytes()));
        let mut url = self.metadata.authorization_endpoint.clone();
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("response_type", "code")
                .append_pair("client_id", &self.client_id)
                .append_pair("redirect_uri", &self.redirect_uri)
                .append_pair("code_challenge", &challenge)
                .append_pair("code_challenge_method", "S256")
                .append_pair("state", &self.state)
                .append_pair("resource", &self.metadata.resource);
            if let Some(scope) = &self.metadata.scope {
                query.append_pair("scope", scope);
            }
        }
        url.to_string()
    }

    /// The callback's redirect path and `state` match this attempt.
    pub(crate) fn owns(&self, callback: &Url) -> bool {
        let path = Url::parse(&self.redirect_uri).map(|url| url.path().to_owned());
        path.is_ok_and(|path| path == callback.path())
            && callback
                .query_pairs()
                .any(|(key, value)| key == "state" && value == self.state.as_str())
    }

    /// Checks the callback against this attempt and returns its code.
    pub(crate) fn code_from(&self, callback: &Url) -> Result<String> {
        let expected = Url::parse(&self.redirect_uri).map_err(|_| anyhow!("invalid redirect"))?;
        ensure!(
            callback.path() == expected.path(),
            "the sign-in callback URL is not this sign-in's redirect"
        );
        let pairs: Vec<(String, String)> = callback.query_pairs().into_owned().collect();
        let get = |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        };
        ensure!(
            get("state") == Some(self.state.as_str()),
            "the sign-in callback does not belong to the current sign-in"
        );
        if let Some(error) = get("error") {
            let code =
                error_code(&json!({ "error": error })).unwrap_or_else(|| "unknown_error".into());
            bail!("the authorization server refused the sign-in ({code})");
        }
        if let Some(issuer) = get("iss") {
            ensure!(
                issuer == self.metadata.issuer,
                "the sign-in callback names a different authorization server"
            );
        }
        get("code")
            .filter(|code| !code.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("the sign-in callback has no authorization code"))
    }

    pub(crate) async fn exchange(&self, client: &Client, code: &str) -> Result<Tokens> {
        let mut pairs = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("client_id", self.client_id.as_str()),
            ("code_verifier", self.verifier.as_str()),
            ("resource", self.metadata.resource.as_str()),
        ];
        if let Some(secret) = &self.client_secret {
            pairs.push(("client_secret", secret));
        }
        let (status, body) = post_form(client, &self.metadata.token_endpoint, &pairs).await?;
        let body = body.unwrap_or(Value::Null);
        if !(200..300).contains(&status) {
            match error_code(&body) {
                Some(code) => bail!("the sign-in code exchange was refused ({code})"),
                None => bail!("the sign-in code exchange was refused (status {status})"),
            }
        }
        let template = Tokens {
            server_url: self.server_url.clone(),
            resource: self.metadata.resource.clone(),
            issuer: self.metadata.issuer.clone(),
            token_endpoint: self.metadata.token_endpoint.to_string(),
            client_id: self.client_id.clone(),
            client_secret: self.client_secret.clone(),
            access_token: String::new(),
            refresh_token: None,
            expires_at: None,
        };
        tokens_from(&body, template)
            .ok_or_else(|| anyhow!("the sign-in code exchange returned no usable token"))
    }
}

/// Why a refresh did not produce tokens.
pub(crate) enum RefreshError {
    /// The authorization server refused the grant: sign in again.
    Rejected,
    /// Network or server trouble: the tokens may still be good later.
    Failed(String),
}

pub(crate) async fn refresh(client: &Client, tokens: &Tokens) -> Result<Tokens, RefreshError> {
    let Some(refresh_token) = &tokens.refresh_token else {
        return Err(RefreshError::Rejected);
    };
    let url = endpoint(&tokens.token_endpoint, "token_endpoint")
        .map_err(|error| RefreshError::Failed(error.to_string()))?;
    let mut pairs = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token.as_str()),
        ("client_id", tokens.client_id.as_str()),
        ("resource", tokens.resource.as_str()),
    ];
    if let Some(secret) = &tokens.client_secret {
        pairs.push(("client_secret", secret));
    }
    let (status, body) = post_form(client, &url, &pairs)
        .await
        .map_err(|error| RefreshError::Failed(error.to_string()))?;
    let body = body.unwrap_or(Value::Null);
    match status {
        200..=299 => tokens_from(&body, tokens.clone()).ok_or_else(|| {
            RefreshError::Failed("the token refresh returned no usable token".into())
        }),
        400 | 401 => Err(RefreshError::Rejected),
        _ => Err(RefreshError::Failed(format!(
            "the token refresh failed (status {status})"
        ))),
    }
}

/// A refresh may omit a rotated refresh token; the previous one then stays.
fn tokens_from(body: &Value, mut tokens: Tokens) -> Option<Tokens> {
    let text = |value: &Value| {
        value
            .as_str()
            .filter(|text| {
                !text.is_empty()
                    && text.len() <= 16_384
                    && text.bytes().all(|b| b.is_ascii_graphic())
            })
            .map(str::to_owned)
    };
    if let Some(kind) = body["token_type"].as_str()
        && !kind.eq_ignore_ascii_case("bearer")
    {
        return None;
    }
    tokens.access_token = text(&body["access_token"])?;
    if let Some(refresh) = text(&body["refresh_token"]) {
        tokens.refresh_token = Some(refresh);
    }
    tokens.expires_at = body["expires_in"]
        .as_u64()
        .or_else(|| body["expires_in"].as_f64().map(|seconds| seconds as u64))
        .filter(|seconds| *seconds > 0)
        .map(|seconds| now() + seconds.min(366 * 24 * 3600));
    Some(tokens)
}

/// A public client (`token_endpoint_auth_method = none`) registered for this
/// sign-in's loopback redirect. Without a registration endpoint there is no
/// client to sign in with.
async fn register(
    client: &Client,
    metadata: &Metadata,
    redirect_uri: &str,
) -> Result<(String, Option<String>)> {
    let Some(url) = &metadata.registration_endpoint else {
        bail!("the MCP server's authorization server does not offer dynamic client registration");
    };
    let body = json!({
        "client_name": "BubblePaw",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let response = client
        .post(url.clone())
        .header(ACCEPT, HeaderValue::from_static("application/json"))
        .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
        .body(body.to_string())
        .send()
        .await
        .map_err(|_| anyhow!("MCP OAuth client registration failed"))?;
    let status = response.status().as_u16();
    let body = read_json(response).await.unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        match error_code(&body) {
            Some(code) => bail!("MCP OAuth client registration was refused ({code})"),
            None => bail!("MCP OAuth client registration was refused (status {status})"),
        }
    }
    let client_id = body["client_id"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 1024)
        .ok_or_else(|| anyhow!("MCP OAuth client registration returned no client_id"))?;
    let secret = body["client_secret"]
        .as_str()
        .filter(|secret| !secret.is_empty() && secret.len() <= 4096)
        .map(str::to_owned);
    Ok((client_id.to_owned(), secret))
}
