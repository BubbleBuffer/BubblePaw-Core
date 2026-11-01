//! Sign-in controls. The host never opens a browser: `auth_start` returns the
//! authorization URL for the client to open, and the code arrives on the
//! loopback redirect or through `auth_complete` with the pasted callback URL.
//! A completed sign-in restarts the server so its tools are listed.
use super::*;
use crate::oauth::ServerAuth;

const MAX_HEADER_VALUE: usize = 8 * 1024;
const MAX_HEADERS: usize = 32;

impl HostInner {
    /// The server's shared sign-in state, created on first use and kept across
    /// restarts. `None` for servers without OAuth.
    pub(super) fn auth_for(
        self: &Arc<Self>,
        name: &str,
        config: &McpServerSettings,
    ) -> Result<Option<Arc<ServerAuth>>> {
        let Some(url) = config.url.as_deref().filter(|_| config.oauth) else {
            self.auth.lock().unwrap().remove(name);
            return Ok(None);
        };
        let dir = self
            .auth_dir
            .as_deref()
            .ok_or_else(|| anyhow!("MCP OAuth needs a data directory for its tokens"))?;
        let mut auth = self.auth.lock().unwrap();
        if let Some(existing) = auth.get(name)
            && existing.server_url() == url
        {
            return Ok(Some(existing.clone()));
        }
        if let Some(old) = auth.remove(name) {
            old.close();
        }
        let host = Arc::downgrade(self);
        let changed: crate::oauth::Changed = Arc::new(move |_name: &str| {
            if let Some(host) = host.upgrade() {
                host.publish();
            }
        });
        let created = ServerAuth::new(name, url, dir, changed)?;
        auth.insert(name.to_owned(), created.clone());
        Ok(Some(created))
    }
}

impl HostInner {
    /// The server's secret file, when the host has a data directory.
    pub(super) fn secrets_path(&self, name: &str) -> Option<PathBuf> {
        self.auth_dir
            .as_deref()
            .map(|dir| crate::oauth::store::path(dir, name))
    }

    /// Deletes a removed server's secret file: tokens and header values.
    pub(super) fn forget_secrets(&self, name: &str) {
        self.header_names.lock().unwrap().remove(name);
        if let Some(path) = self.secrets_path(name) {
            let _lock = crate::oauth::store::lock(&path);
            let _ = crate::oauth::store::remove_all(&path);
        }
    }

    /// Forgets the OAuth tokens of every server whose `url` changed or that
    /// stopped being a URL server: they belong to the old resource. Stored
    /// headers stay. Servers new to `settings` have nothing to forget.
    pub(super) fn drop_stale_tokens(&self, settings: &McpSettings) {
        let previous = self.state.lock().unwrap().settings.clone();
        for (name, config) in &settings.servers {
            let Some(old) = previous
                .servers
                .get(name)
                .and_then(|old| old.url.as_deref())
            else {
                continue;
            };
            if config.url.as_deref() == Some(old) {
                continue;
            }
            if let Some(auth) = self.auth.lock().unwrap().remove(name) {
                auth.close();
            }
            if let Some(path) = self.secrets_path(name) {
                let _lock = crate::oauth::store::lock(&path);
                let _ = crate::oauth::store::remove(&path);
            }
        }
    }

    /// Re-reads which header secrets each HTTP server has (names only).
    pub(super) fn load_header_names(&self, settings: &McpSettings) {
        let names = settings
            .servers
            .iter()
            .filter(|(_, config)| config.url.is_some())
            .filter_map(|(name, _)| {
                let path = self.secrets_path(name)?;
                let names: Vec<String> = crate::oauth::store::read_headers(&path)
                    .into_keys()
                    .collect();
                (!names.is_empty()).then(|| (name.clone(), names))
            })
            .collect();
        *self.header_names.lock().unwrap() = names;
    }
}

impl McpHost {
    /// Stores (`Some`) or removes (`None`) one header value for an HTTP server
    /// in its secret file, then reconnects the server. The value never appears
    /// in errors, status or logs.
    pub async fn set_header(&self, server: &str, header: &str, value: Option<&str>) -> Result<()> {
        ensure!(
            !self.inner.closed.load(Ordering::SeqCst),
            "MCP host shut down"
        );
        let config = self
            .inner
            .state
            .lock()
            .unwrap()
            .settings
            .servers
            .get(server)
            .cloned()
            .ok_or_else(|| anyhow!("MCP server {server} is not configured"))?;
        ensure!(
            config.url.is_some(),
            "MCP server {server} is not an HTTP server"
        );
        ensure!(
            reqwest::header::HeaderName::from_bytes(header.as_bytes()).is_ok(),
            "header must be a valid HTTP header name"
        );
        ensure!(
            !crate::config::reserved_header(&header.to_ascii_lowercase()),
            "header is a reserved protocol header"
        );
        if let Some(value) = value {
            ensure!(!value.is_empty(), "header value must not be empty");
            ensure!(
                value.len() <= MAX_HEADER_VALUE
                    && reqwest::header::HeaderValue::from_str(value).is_ok(),
                "header value is not a valid HTTP header value"
            );
            ensure!(
                !(config.oauth && header.eq_ignore_ascii_case("authorization")),
                "oauth cannot be combined with an Authorization header"
            );
        }
        let path = self
            .inner
            .secrets_path(server)
            .ok_or_else(|| anyhow!("MCP header secrets need a data directory"))?;
        let (header, value) = (header.to_owned(), value.map(str::to_owned));
        let names = tokio::task::spawn_blocking(move || {
            let lock = crate::oauth::store::lock(&path)?;
            let mut names: Vec<String> = crate::oauth::store::read_headers(&path)
                .into_keys()
                .filter(|known| !known.eq_ignore_ascii_case(&header))
                .collect();
            ensure!(
                value.is_none() || names.len() < MAX_HEADERS,
                "too many stored MCP headers"
            );
            crate::oauth::store::set_header(&path, &header, value.as_deref())?;
            if value.is_some() {
                names.push(header);
            }
            names.sort();
            drop(lock);
            Ok::<_, anyhow::Error>(names)
        })
        .await
        .map_err(|_| anyhow!("cannot store the MCP header"))??;
        {
            let mut cache = self.inner.header_names.lock().unwrap();
            if names.is_empty() {
                cache.remove(server);
            } else {
                cache.insert(server.to_owned(), names);
            }
        }
        restart(Arc::downgrade(&self.inner), server).await;
        Ok(())
    }

    fn signed_in_server(&self, server: &str) -> Result<Arc<ServerAuth>> {
        let config = self
            .inner
            .state
            .lock()
            .unwrap()
            .settings
            .servers
            .get(server)
            .cloned()
            .ok_or_else(|| anyhow!("MCP server {server} is not configured"))?;
        ensure!(
            config.url.is_some() && config.oauth,
            "MCP server {server} does not use OAuth"
        );
        self.inner
            .auth_for(server, &config)?
            .ok_or_else(|| anyhow!("MCP server {server} does not use OAuth"))
    }

    /// Starts a sign-in and returns the authorization URL. Any earlier waiting
    /// sign-in for the server is cancelled.
    pub async fn auth_start(&self, server: &str) -> Result<String> {
        ensure!(
            !self.inner.closed.load(Ordering::SeqCst),
            "MCP host shut down"
        );
        let auth = self.signed_in_server(server)?;
        let (url, pending, loopback, cancel) = auth.start().await?;
        let host = Arc::downgrade(&self.inner);
        let name = server.to_owned();
        self.inner.background.lock().unwrap().spawn(async move {
            if auth.wait(pending, loopback, cancel).await {
                restart(host, &name).await;
            }
        });
        Ok(url)
    }

    /// Completes the waiting sign-in with the callback URL the browser reached,
    /// for clients the loopback redirect cannot reach.
    pub async fn auth_complete(&self, server: &str, redirect_url: &str) -> Result<()> {
        let auth = self.signed_in_server(server)?;
        auth.complete(redirect_url).await?;
        restart(Arc::downgrade(&self.inner), server).await;
        Ok(())
    }

    /// Forgets the server's tokens and cancels a waiting sign-in.
    pub async fn auth_sign_out(&self, server: &str) -> Result<()> {
        let auth = self.signed_in_server(server)?;
        auth.sign_out().await?;
        restart(Arc::downgrade(&self.inner), server).await;
        Ok(())
    }
}

/// Restart one server with the host's current settings so a sign-in change
/// takes effect. A disabled server stays stopped.
async fn restart(host: Weak<HostInner>, name: &str) {
    let Some(inner) = host.upgrade() else { return };
    let host = McpHost {
        inner,
        owner: false,
    };
    let settings = host.inner.state.lock().unwrap().settings.clone();
    if settings.servers.get(name).is_some_and(|c| c.enabled) {
        // The next `refresh_tools` reports the new tools to the registry owner.
        if let Ok(diff) = host.reload(&settings, Some(name)).await {
            host.inner.state.lock().unwrap().pending.absorb(diff);
        }
    }
    host.inner.publish();
}
