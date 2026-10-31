//! Sign-in controls. The host never opens a browser: `auth_start` returns the
//! authorization URL for the client to open, and the code arrives on the
//! loopback redirect or through `auth_complete` with the pasted callback URL.
//! A completed sign-in restarts the server so its tools are listed.
use super::*;
use crate::oauth::ServerAuth;

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

impl McpHost {
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
