//! One OAuth-enabled server's sign-in state, shared by every connection
//! generation of that server in a host. Tokens are re-read from the store on
//! each request so a sign-in or rotation by another process is adopted.
use super::{
    Challenge, discovery, flow, flow::Pending, flow::RefreshError, http_client, loopback,
    loopback::Loopback, now, store, store::Tokens,
};
use crate::McpAuthState;
use anyhow::{Result, anyhow, bail};
use bubblepaw_core::CancellationToken;
use reqwest::{
    Client, Url,
    header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue, WWW_AUTHENTICATE},
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

/// How long a started sign-in waits for its loopback redirect.
pub(crate) const SIGN_IN_WINDOW: Duration = Duration::from_secs(10 * 60);

pub(crate) type Changed = Arc<dyn Fn(&str) + Send + Sync>;

pub(crate) struct ServerAuth {
    pub name: String,
    path: PathBuf,
    client: Client,
    inner: Mutex<Inner>,
    refresh_gate: tokio::sync::Mutex<()>,
    changed: Changed,
}

struct Inner {
    server_url: String,
    state: McpAuthState,
    error: Option<String>,
    challenge: Option<Challenge>,
    /// Every token value seen, longest first, for redaction.
    secrets: Vec<String>,
    force_refresh: bool,
    retry: bool,
    pending: Option<(Arc<Pending>, CancellationToken)>,
}

impl ServerAuth {
    pub(crate) fn new(
        name: &str,
        server_url: &str,
        dir: &std::path::Path,
        changed: Changed,
    ) -> Result<Arc<Self>> {
        let path = store::path(dir, name);
        let tokens = store::read(&path).filter(|tokens| tokens.server_url == server_url);
        let auth = Arc::new(Self {
            name: name.to_owned(),
            path,
            client: http_client()?,
            inner: Mutex::new(Inner {
                server_url: server_url.to_owned(),
                state: if tokens.is_some() {
                    McpAuthState::SignedIn
                } else {
                    McpAuthState::NeedsSignIn
                },
                error: None,
                challenge: None,
                secrets: Vec::new(),
                force_refresh: false,
                retry: false,
                pending: None,
            }),
            refresh_gate: tokio::sync::Mutex::new(()),
            changed,
        });
        if let Some(tokens) = &tokens {
            auth.remember(tokens);
        }
        Ok(auth)
    }

    pub(crate) fn server_url(&self) -> String {
        self.inner.lock().unwrap().server_url.clone()
    }

    pub(crate) fn status(&self) -> (McpAuthState, Option<String>) {
        let inner = self.inner.lock().unwrap();
        (inner.state, inner.error.clone())
    }

    fn set_state(&self, state: McpAuthState, error: Option<String>) {
        let changed = {
            let mut inner = self.inner.lock().unwrap();
            let changed = inner.state != state || inner.error != error;
            inner.state = state;
            inner.error = error;
            changed
        };
        if changed {
            (self.changed)(&self.name);
        }
    }

    fn remember(&self, tokens: &Tokens) {
        let mut inner = self.inner.lock().unwrap();
        for secret in tokens.secrets() {
            if !inner.secrets.iter().any(|known| known == secret) {
                inner.secrets.push(secret.to_owned());
            }
        }
        inner
            .secrets
            .sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    }

    pub(crate) fn redact(&self, text: &str) -> String {
        let inner = self.inner.lock().unwrap();
        let mut text = text.to_owned();
        for secret in &inner.secrets {
            text = text.replace(secret.as_str(), "[redacted]");
        }
        text
    }

    fn stored(&self) -> Option<Tokens> {
        let url = self.server_url();
        let tokens = store::read(&self.path).filter(|tokens| tokens.server_url == url)?;
        self.remember(&tokens);
        Some(tokens)
    }

    /// The Authorization header for the next request, refreshing first when the
    /// access token is about to expire or was rejected. `None` sends the request
    /// unauthenticated, so the server's challenge is recorded for sign-in.
    pub(crate) async fn bearer(&self) -> Result<Option<HeaderValue>> {
        let Some(mut tokens) = self.stored() else {
            let state = self.status().0;
            if matches!(state, McpAuthState::SignedIn) {
                self.set_state(McpAuthState::NeedsSignIn, None);
            }
            return Ok(None);
        };
        let forced = std::mem::take(&mut self.inner.lock().unwrap().force_refresh);
        if forced || tokens.expiring(now()) {
            tokens = match self.refresh(&tokens).await {
                Ok(tokens) => tokens,
                Err(RefreshError::Rejected) => {
                    self.set_state(McpAuthState::Expired, None);
                    return Ok(None);
                }
                Err(RefreshError::Failed(message)) => bail!("MCP OAuth {message}"),
            };
        }
        if self.status().0 != McpAuthState::Waiting {
            self.set_state(McpAuthState::SignedIn, None);
        }
        let mut value = HeaderValue::from_str(&format!("Bearer {}", tokens.access_token))
            .map_err(|_| anyhow!("invalid MCP OAuth access token"))?;
        value.set_sensitive(true);
        Ok(Some(value))
    }

    /// Refresh under the in-process gate and the cross-process lock file. A
    /// rotation another process already made is adopted instead of spending
    /// the refresh token again.
    async fn refresh(&self, used: &Tokens) -> Result<Tokens, RefreshError> {
        let _gate = self.refresh_gate.lock().await;
        let path = self.path.clone();
        let lock = tokio::task::spawn_blocking(move || store::lock(&path))
            .await
            .map_err(|_| RefreshError::Failed("token lock failed".into()))?
            .map_err(|error| RefreshError::Failed(error.to_string()))?;
        if let Some(current) = self.stored()
            && current.access_token != used.access_token
            && !current.expiring(now())
        {
            return Ok(current);
        }
        let refreshed = flow::refresh(&self.client, used).await?;
        self.remember(&refreshed);
        store::write(&self.path, &refreshed)
            .map_err(|error| RefreshError::Failed(error.to_string()))?;
        drop(lock);
        Ok(refreshed)
    }

    /// Records a 401 and returns the diagnostic for it. A rejected access token
    /// is refreshed before the next request; a read-only discovery retries once.
    pub(crate) fn unauthorized(&self, headers: &HeaderMap, sent_token: bool) -> String {
        let challenge = headers
            .get_all(WWW_AUTHENTICATE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find_map(Challenge::parse);
        let state = {
            let mut inner = self.inner.lock().unwrap();
            if challenge.is_some() {
                inner.challenge = challenge;
            }
            if sent_token {
                inner.force_refresh = true;
                inner.retry = true;
            }
            inner.state
        };
        match state {
            McpAuthState::Expired => "MCP HTTP sign-in expired; sign in again".into(),
            _ if sent_token => "MCP HTTP authorization was rejected".into(),
            McpAuthState::Waiting => "MCP HTTP sign-in required".into(),
            _ => {
                self.set_state(McpAuthState::NeedsSignIn, None);
                "MCP HTTP sign-in required".into()
            }
        }
    }

    /// True once after a rejected access token, for one read-only retry.
    pub(crate) fn take_retry(&self) -> bool {
        std::mem::take(&mut self.inner.lock().unwrap().retry)
    }

    /// Discover, register and listen; returns the authorization URL. The caller
    /// runs `wait` with the returned pieces.
    pub(crate) async fn start(
        &self,
    ) -> Result<(String, Arc<Pending>, Loopback, CancellationToken)> {
        self.cancel_pending();
        let result = async {
            let server_url = self.server_url();
            let server = Url::parse(&server_url).map_err(|_| anyhow!("invalid MCP HTTP URL"))?;
            let challenge = self.inner.lock().unwrap().challenge.clone();
            let challenge = match challenge {
                Some(challenge) => Some(challenge),
                None => self.probe(&server).await,
            };
            let metadata = discovery::discover(&self.client, &server, challenge.as_ref()).await?;
            let loopback = Loopback::bind().await?;
            let pending = Arc::new(
                Pending::new(
                    &self.client,
                    server_url,
                    metadata,
                    loopback.redirect_uri.clone(),
                )
                .await?,
            );
            Ok::<_, anyhow::Error>((pending, loopback))
        }
        .await;
        let (pending, loopback) = match result {
            Ok(found) => found,
            Err(error) => {
                self.set_state(McpAuthState::Failed, Some(error.to_string()));
                return Err(error);
            }
        };
        let cancel = CancellationToken::new();
        self.inner.lock().unwrap().pending = Some((pending.clone(), cancel.clone()));
        self.set_state(McpAuthState::Waiting, None);
        Ok((pending.authorization_url(), pending, loopback, cancel))
    }

    /// An unauthenticated request, only to read the server's challenge.
    async fn probe(&self, server: &Url) -> Option<Challenge> {
        let response = self
            .client
            .post(server.clone())
            .header(
                ACCEPT,
                HeaderValue::from_static("application/json, text/event-stream"),
            )
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .header(
                "mcp-protocol-version",
                HeaderValue::from_static("2025-11-25"),
            )
            .body(r#"{"jsonrpc":"2.0","id":0,"method":"ping"}"#)
            .send()
            .await
            .ok()?;
        response
            .headers()
            .get_all(WWW_AUTHENTICATE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find_map(Challenge::parse)
    }

    /// Waits for the loopback redirect of this sign-in. Requests for another
    /// path or state are answered and ignored. Returns true once signed in.
    pub(crate) async fn wait(
        &self,
        pending: Arc<Pending>,
        loopback: Loopback,
        cancel: CancellationToken,
    ) -> bool {
        let waited = tokio::time::timeout(SIGN_IN_WINDOW, async {
            loop {
                let (url, stream) = tokio::select! {
                    _ = cancel.cancelled() => return None,
                    next = loopback.next() => next,
                };
                if !pending.owns(&url) {
                    loopback::answer(stream, false).await;
                    continue;
                }
                let result = self.complete_with(&pending, &url).await;
                loopback::answer(stream, result.is_ok()).await;
                return Some(result.is_ok());
            }
        })
        .await;
        match waited {
            Ok(Some(signed_in)) => signed_in,
            Ok(None) => false,
            Err(_) => {
                if self.take_pending(&pending) {
                    self.set_state(McpAuthState::Failed, Some("sign-in timed out".into()));
                }
                false
            }
        }
    }

    /// The pasted-URL completion for clients the loopback redirect cannot reach.
    pub(crate) async fn complete(&self, redirect_url: &str) -> Result<()> {
        let pending = self
            .inner
            .lock()
            .unwrap()
            .pending
            .as_ref()
            .map(|(pending, _)| pending.clone())
            .ok_or_else(|| anyhow!("no sign-in is waiting for this MCP server"))?;
        let url = Url::parse(redirect_url.trim())
            .map_err(|_| anyhow!("the sign-in callback is not a valid URL"))?;
        self.complete_with(&pending, &url).await
    }

    async fn complete_with(&self, pending: &Arc<Pending>, url: &Url) -> Result<()> {
        // A callback that does not belong to this sign-in leaves it waiting.
        if !pending.owns(url) {
            bail!("the sign-in callback does not belong to the current sign-in");
        }
        if !self.take_pending(pending) {
            bail!("this sign-in was already completed or cancelled");
        }
        let code = match pending.code_from(url) {
            Ok(code) => code,
            Err(error) => {
                self.set_state(McpAuthState::Failed, Some(error.to_string()));
                return Err(error);
            }
        };
        let tokens = match pending.exchange(&self.client, &code).await {
            Ok(tokens) => tokens,
            Err(error) => {
                self.set_state(McpAuthState::Failed, Some(error.to_string()));
                return Err(error);
            }
        };
        self.remember(&tokens);
        let path = self.path.clone();
        let written = tokio::task::spawn_blocking(move || {
            let lock = store::lock(&path)?;
            store::write(&path, &tokens)?;
            drop(lock);
            Ok::<_, anyhow::Error>(())
        })
        .await
        .map_err(|_| anyhow!("cannot store MCP OAuth tokens"))
        .and_then(|result| result);
        if let Err(error) = written {
            self.set_state(McpAuthState::Failed, Some(error.to_string()));
            return Err(error);
        }
        {
            let mut inner = self.inner.lock().unwrap();
            inner.force_refresh = false;
        }
        self.set_state(McpAuthState::SignedIn, None);
        Ok(())
    }

    /// Clears the pending sign-in if it is still `pending`; true when it was.
    fn take_pending(&self, pending: &Arc<Pending>) -> bool {
        let mut inner = self.inner.lock().unwrap();
        match &inner.pending {
            Some((current, cancel)) if Arc::ptr_eq(current, pending) => {
                cancel.cancel();
                inner.pending = None;
                true
            }
            _ => false,
        }
    }

    fn cancel_pending(&self) -> bool {
        match self.inner.lock().unwrap().pending.take() {
            Some((_, cancel)) => {
                cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// Forgets the tokens and cancels a waiting sign-in.
    pub(crate) async fn sign_out(&self) -> Result<()> {
        self.cancel_pending();
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let lock = store::lock(&path)?;
            store::remove(&path)?;
            drop(lock);
            Ok::<_, anyhow::Error>(())
        })
        .await
        .map_err(|_| anyhow!("cannot remove MCP OAuth tokens"))??;
        self.inner.lock().unwrap().force_refresh = false;
        self.set_state(McpAuthState::NeedsSignIn, None);
        Ok(())
    }

    /// Stops a waiting sign-in when the host closes.
    pub(crate) fn close(&self) {
        self.cancel_pending();
    }
}
