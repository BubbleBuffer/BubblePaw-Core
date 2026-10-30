//! The host-level context: the `stdio` command's setup, kept for the host's
//! lifetime so JSON-RPC sessions on the control endpoint share it.
use super::*;
use crate::stdio_transport;

/// How the `stdio` command runs: as the host-level context, which publishes
/// its [`HostCtx`] and optionally serves stdio on it.
pub struct HostContextMode {
    pub(super) publish: tokio::sync::oneshot::Sender<HostCtx>,
    /// Serve stdio with this cancellation, and report its result here.
    pub(super) stdio: Option<StdioPrimary>,
}

/// The host's own `stdio` command, served on the host context.
pub type StdioPrimary = (tokio::sync::oneshot::Sender<Result<()>>, CancellationToken);

/// The host-level context: built once from the host's own configuration, it
/// answers JSON-RPC sessions and outlives every single request.
pub struct HostContext {
    ctx: HostCtx,
    stop: CancellationToken,
    task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<Result<()>>>>,
}

impl HostContext {
    /// Build the context from `request` with the setup of the `stdio` command.
    /// A different command is not run: the context runs no prompt and opens no
    /// trace of its own. With `stdio`, the host's `stdio` command is served on
    /// this context and its result is sent once it ends.
    pub async fn start(
        mut request: PreparedRequest,
        store: Store,
        lifetime: HostLifetime,
        stdio: Option<StdioPrimary>,
    ) -> Result<Self> {
        if !matches!(request.cli.command, Command::Stdio) {
            request.cli.command = Command::Stdio;
            request.cli.trace_file = None;
            request.delegation = None;
            request.task_prompt = None;
        }
        let (publish, published) = tokio::sync::oneshot::channel();
        let stop = CancellationToken::new();
        let task = tokio::spawn(Box::pin(execute(
            request,
            store,
            lifetime,
            Output::default(),
            stop.clone(),
            Some(HostContextMode { publish, stdio }),
        )));
        match published.await {
            Ok(ctx) => Ok(Self {
                ctx,
                stop,
                task: tokio::sync::Mutex::new(Some(task)),
            }),
            Err(_) => Err(match task.await {
                Ok(Err(error)) => error,
                Ok(Ok(())) => anyhow::anyhow!("host context stopped before it started"),
                Err(error) => anyhow::Error::new(error).context("host context failed"),
            }),
        }
    }

    /// The shared context. Clone it per session.
    pub fn ctx(&self) -> &HostCtx {
        &self.ctx
    }

    /// Stop the context and wait for its request cleanup. Later calls return
    /// at once.
    pub async fn shutdown(&self) -> Result<()> {
        self.stop.cancel();
        match self.task.lock().await.take() {
            Some(task) => task.await.context("host context failed")?,
            None => Ok(()),
        }
    }
}

/// Wake-ups and child answers for the host context, then the optional stdio
/// command, then wait until the host stops the context.
pub(super) async fn host_context(
    ctx: HostCtx,
    mode: Option<HostContextMode>,
    stop: &CancellationToken,
) -> Result<()> {
    let mode = mode.context("stdio runs only as the host context")?;
    let harness = ctx.harness.clone();
    let wake = ctx
        .wake_coordinator()
        .context("the host context needs the wake coordinator")?;
    let questions = ctx
        .questions
        .as_ref()
        .context("the host context needs question tools")?;
    questions.set_child_answer_harness(harness.clone())?;
    let wake_registration = wake.register_harness_scoped(harness, ctx.scope.clone());
    let wake_driver = wake.start_driver();
    let _ = mode.publish.send(ctx.clone());
    if let Some((done, cancellation)) = mode.stdio {
        let _ = done.send(stdio_transport::run(ctx, cancellation).await);
    }
    stop.cancelled().await;
    wake_driver.shutdown().await;
    drop(wake_registration);
    Ok(())
}
