use super::{Limits, PermissiveAdmission, RelayAdmission, RelayRouter};
use crate::Result;
use std::{net::SocketAddr, sync::Arc};
use tokio::{
    net::TcpListener,
    sync::oneshot,
    task::{JoinHandle, JoinSet},
};

/// Optional observer receives only forwarded encrypted data frames.
pub type CiphertextObserver = Arc<dyn Fn(&[u8]) + Send + Sync>;

#[derive(Clone)]
pub struct RelayConfig {
    pub limits: Limits,
    pub ciphertext_observer: Option<CiphertextObserver>,
    pub admission: Arc<dyn RelayAdmission>,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            limits: Limits::default(),
            ciphertext_observer: None,
            admission: Arc::new(PermissiveAdmission),
        }
    }
}

/// Owns the listener and every accepted connection task.
pub struct Relay {
    address: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl Relay {
    pub async fn bind(address: SocketAddr, config: RelayConfig) -> Result<Self> {
        let router = RelayRouter::new(config)?;
        let listener = TcpListener::bind(address).await?;
        let address = listener.local_addr()?;
        let (shutdown, stop) = oneshot::channel();
        let task = tokio::spawn(serve(listener, router, stop));
        Ok(Self {
            address,
            shutdown: Some(shutdown),
            task: Some(task),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    pub async fn shutdown(mut self) {
        if let Some(stop) = self.shutdown.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        if let Some(stop) = self.shutdown.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn serve(listener: TcpListener, router: RelayRouter, mut stop: oneshot::Receiver<()>) {
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut stop => break,
            _ = tasks.join_next(), if !tasks.is_empty() => {},
            accepted = listener.accept() => {
                let Ok((stream, address)) = accepted else { break; };
                let router = router.clone();
                tasks.spawn(async move { let _ = router.serve_tcp(stream, address.ip()).await; });
            }
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    router.shutdown().await;
}
