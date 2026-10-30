//! One client for a BubblePaw host, embedded in this process or attached to
//! the host that owns a data directory. Every call goes to the host-API method
//! table through the same session loop as `bubblepaw stdio`.
use crate::host::{Host, HostBuilder};
use bubblepaw_core::{CancellationToken, Store};
use bubblepaw_runner::{
    host_attach,
    host_client::HostClient,
    host_lifetime::HostLifetime,
    host_transport::{self, HostEndpoint},
};
use serde_json::Value;
use std::{path::Path, time::Duration};

pub use bubblepaw_runner::host_client::{ClientError, Notifications};

/// How long `attach` waits for a host that is still starting.
const ATTACH_WAIT: Duration = Duration::from_secs(5);

/// A host-API client. Embedded, it owns the host: it publishes the control
/// endpoint and `host.json` so other clients and `bubblepaw run` attach to it,
/// and `close` stops it. Attached, `close` only ends the session.
///
/// Every constructor and method must run inside a Tokio runtime.
pub struct Client {
    session: HostClient,
    embedded: Option<Embedded>,
}

struct Embedded {
    host: Host,
    lifetime: HostLifetime,
    stop: CancellationToken,
    endpoint: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl Client {
    /// Build the host and serve it from this process. A store with a data
    /// directory publishes its endpoint there, as a CLI host does; an
    /// in-memory store serves this client only.
    pub async fn embedded(builder: HostBuilder) -> Result<Self, ClientError> {
        let host = builder.build().map_err(host_error)?;
        let lifetime = HostLifetime::default();
        lifetime.register(host.harness.clone());
        let stop = CancellationToken::new();
        let endpoint = match host.store.data_dir() {
            Some(data_dir) => {
                let endpoint = HostEndpoint::bind(data_dir).await.map_err(host_error)?;
                Some(tokio::spawn(host_attach::serve_embedded(
                    endpoint,
                    host.store.clone(),
                    lifetime.clone(),
                    host.api().clone(),
                    stop.clone(),
                )))
            }
            None => None,
        };
        Ok(Self {
            session: HostClient::in_process(host.api().clone()),
            embedded: Some(Embedded {
                host,
                lifetime,
                stop,
                endpoint,
            }),
        })
    }

    /// Attach to the host, CLI or embedded, that owns `data_dir`.
    pub async fn attach(data_dir: impl AsRef<Path>) -> Result<Self, ClientError> {
        Ok(Self {
            session: HostClient::attach(data_dir.as_ref(), ATTACH_WAIT).await?,
            embedded: None,
        })
    }

    /// Attach when another process owns `data_dir`; otherwise open its store,
    /// pass it to `build` and embed the host. `build` runs only when this
    /// process becomes the owner.
    pub async fn open(
        data_dir: impl AsRef<Path>,
        build: impl FnOnce(Store) -> anyhow::Result<HostBuilder>,
    ) -> Result<Self, ClientError> {
        let data_dir = data_dir.as_ref();
        let deadline = tokio::time::Instant::now() + ATTACH_WAIT;
        loop {
            // Store::open shares a store this process already owns, so a host
            // this process serves is found by its published pid instead.
            let own = host_transport::read_host_metadata(data_dir)
                .ok()
                .flatten()
                .is_some_and(|host| host.pid == std::process::id());
            if own {
                return Self::attach(data_dir).await;
            }
            match Store::open_data_dir(data_dir) {
                Ok(store) => return Self::embedded(build(store).map_err(host_error)?).await,
                Err(error) if host_attach::is_owned(&error) => {
                    // The owner may still be publishing, or may be stopping.
                    match HostClient::attach(data_dir, Duration::from_millis(100)).await {
                        Ok(session) => {
                            return Ok(Self {
                                session,
                                embedded: None,
                            });
                        }
                        Err(error) if tokio::time::Instant::now() >= deadline => {
                            return Err(error);
                        }
                        Err(_) => continue,
                    }
                }
                Err(error) => return Err(host_error(error)),
            }
        }
    }

    /// Call one host-API method and return its `result`. A request whose
    /// session ends before the reply fails with [`ClientError::Disconnected`]
    /// and is never sent again.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, ClientError> {
        self.session.call(method, params).await
    }

    /// Notifications from now on, in the stdio shapes: `event`, and
    /// `events.gap` (recover with `events.after`).
    pub fn subscribe(&self) -> Notifications {
        self.session.subscribe()
    }

    /// The embedded host, or `None` when attached.
    pub fn host(&self) -> Option<&Host> {
        self.embedded.as_ref().map(|embedded| &embedded.host)
    }

    /// Embedded: end attached sessions, remove `host.json` and stop the host.
    /// Attached: end this session only.
    pub async fn close(self) -> Result<(), ClientError> {
        self.session.close().await;
        let Some(embedded) = self.embedded else {
            return Ok(());
        };
        embedded.stop.cancel();
        let served = match embedded.endpoint {
            Some(endpoint) => endpoint
                .await
                .map_err(|error| ClientError::Host(error.to_string()))?
                .map_err(host_error),
            None => Ok(()),
        };
        let stopped = embedded.lifetime.shutdown().await.map_err(host_error);
        drop(embedded.host);
        served?;
        stopped
    }
}

fn host_error(error: anyhow::Error) -> ClientError {
    ClientError::Host(format!("{error:#}"))
}
