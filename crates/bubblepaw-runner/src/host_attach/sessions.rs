//! Host-API JSON-RPC sessions on the control endpoint, opened with
//! `attach.session`, and the host-level context they share.
use super::*;
use crate::session_host::host_context::HostContext;
use bubblepaw_runner::{
    host_api::{HostCtx, Profile, ReadScope},
    host_transport::HOST_PROTOCOL_VERSION,
    rpc_session,
};
use std::{future::Future, pin::Pin};

/// The host-level context that `attach.session` connections share.
pub type SessionSource =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<HostCtx>> + Send>> + Send + Sync>;

pub(super) type BoxedRun = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

/// A CLI host's own context. `stdio` serves on it from startup; any other
/// command builds it from the same request when the first session attaches.
#[derive(Default)]
pub(super) struct CliContext {
    pub(super) cell: Arc<tokio::sync::OnceCell<HostContext>>,
}

impl CliContext {
    pub(super) fn source(
        &self,
        request: &PreparedRequest,
        store: &Store,
        lifetime: &HostLifetime,
    ) -> SessionSource {
        let (cell, request, store, lifetime) = (
            self.cell.clone(),
            request.clone(),
            store.clone(),
            lifetime.clone(),
        );
        Arc::new(move || {
            let (cell, request, store, lifetime) = (
                cell.clone(),
                request.clone(),
                store.clone(),
                lifetime.clone(),
            );
            Box::pin(async move {
                let context = cell
                    .get_or_try_init(|| HostContext::start(request, store, lifetime, None))
                    .await?;
                Ok(context.ctx().clone())
            })
        })
    }

    /// The host's own request. `stdio` is served on the host context, which
    /// outlives it; any other command runs as a request of its own.
    pub(super) async fn primary(
        &self,
        request: PreparedRequest,
        store: &Store,
        lifetime: &HostLifetime,
        cancel: &CancellationToken,
    ) -> Result<BoxedRun> {
        if !matches!(request.cli.command, Command::Stdio) {
            let (store, lifetime, cancel) = (store.clone(), lifetime.clone(), cancel.clone());
            return Ok(Box::pin(session_host::execute(
                request,
                store,
                lifetime,
                Output::default(),
                cancel,
                None,
            )));
        }
        let (done, finished) = tokio::sync::oneshot::channel();
        let started = HostContext::start(
            request,
            store.clone(),
            lifetime.clone(),
            Some((done, cancel.clone())),
        )
        .await?;
        let _ = self.cell.set(started);
        Ok(Box::pin(async move {
            finished.await.context("host context stopped")?
        }))
    }

    pub(super) async fn shutdown(&self) -> Result<()> {
        match self.cell.get() {
            Some(context) => context.shutdown().await,
            None => Ok(()),
        }
    }
}

/// Serve the control endpoint for a host that is not a CLI request, such as an
/// SDK host: `attach.session` connections share `ctx`, and `attach.submit`
/// requests run as on a CLI host. Returns once `stop` fires and every
/// connection has ended; dropping `endpoint` then removes `host.json`.
pub async fn serve_embedded(
    mut endpoint: HostEndpoint,
    store: Store,
    lifetime: HostLifetime,
    ctx: HostCtx,
    stop: CancellationToken,
) -> Result<()> {
    let sessions: SessionSource = Arc::new(move || {
        let ctx = ctx.clone();
        Box::pin(async move { Ok(ctx) })
    });
    let mut clients = tokio::task::JoinSet::new();
    let result = loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => break Ok(()),
            accepted = endpoint.accept() => match accepted {
                Ok(connection) => {
                    let (store, lifetime, sessions, stop) =
                        (store.clone(), lifetime.clone(), sessions.clone(), stop.clone());
                    clients.spawn(async move {
                        connection_request(connection, store, lifetime, sessions, stop).await
                    });
                }
                Err(error) => break Err(error.context("host control listener failed")),
            },
            _ = clients.join_next(), if !clients.is_empty() => {}
        }
    };
    // A listener failure stops every connection too.
    stop.cancel();
    while clients.join_next().await.is_some() {}
    drop(endpoint);
    result
}

/// One host-API JSON-RPC session on the host context, after `attach.session`.
/// The reply names the protocol version; the shared session loop serves the rest.
pub(super) async fn session_request(
    id: Value,
    reader: BufReader<tokio::io::ReadHalf<Connection>>,
    mut write: tokio::io::WriteHalf<Connection>,
    sessions: SessionSource,
    host_stop: CancellationToken,
) -> Result<()> {
    if !(id.is_string() || id.is_number()) {
        let error = json!({"code":-32602,"message":"a session needs an id"});
        return write_json(
            &mut write,
            &json!({"jsonrpc":"2.0","id":Value::Null,"error":error}),
        )
        .await;
    }
    let started = tokio::select! {
        _ = host_stop.cancelled() => return Ok(()),
        started = sessions() => started,
    };
    let mut ctx = match started {
        Ok(ctx) => ctx,
        Err(error) => {
            let error = json!({"code":-32603,"message":format!("{error:#}")});
            return write_json(&mut write, &json!({"jsonrpc":"2.0","id":id,"error":error})).await;
        }
    };
    ctx.read_scope = ReadScope::Host;
    write_json(
        &mut write,
        &json!({"jsonrpc":"2.0","id":id,"result":{"protocol_version":HOST_PROTOCOL_VERSION}}),
    )
    .await?;
    rpc_session::serve_stream(&ctx, Profile::Full, reader, write, &host_stop).await
}
