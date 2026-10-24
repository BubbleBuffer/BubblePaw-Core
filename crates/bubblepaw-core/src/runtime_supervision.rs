//! Run supervision: the spawned turn is awaited so that a panic settles its run.
//!
//! Without it, `Harness::prompt`, `resume` and `wait_for_idle` wait on a `Notify`
//! that a panicking turn never signals. The supervisor records the failure the
//! way an ordinary run error is recorded and publishes it to every waiter.
use super::*;

impl Harness {
    /// Spawn `drive` under a supervisor that settles the run if it panics.
    pub(super) fn spawn_drive(
        &self,
        run: Run,
        options: RunOptions,
        cancellation: CancellationToken,
        single_tool: Option<SingleToolRun>,
    ) {
        let driver = self.clone();
        let supervisor = self.clone();
        let admitted = run.clone();
        tokio::spawn(async move {
            let drive = tokio::spawn(async move {
                driver.drive(run, options, cancellation, single_tool).await;
            });
            if let Err(join) = drive.await {
                supervisor.settle_panicked_run(admitted, join);
            }
        });
    }
    /// Record a panicked turn as a failed run, then publish it to its waiters.
    fn settle_panicked_run(&self, mut run: Run, join: tokio::task::JoinError) {
        // A drive loop that already advanced to a queued followup leaves that
        // newer run active, so the panic belongs to it rather than to the run
        // admission returned.
        let current = {
            let active = self.state.active.lock().unwrap();
            active
                .get(&run.session_id)
                .map(|value| value.run_id.clone())
                .filter(|id| *id != run.id)
        };
        if let Some(id) = current
            && let Ok(newer) = self.store.run(&id)
        {
            run = newer;
        }
        run.status = "failed".into();
        run.ended_at = Some(now());
        run.error = Some(format!("run panicked: {}", panic_text(join)));
        if let Err(storage_error) = self.store.close_unresolved(&run.session_id, &run.id) {
            let error = run.error.take().unwrap_or_default();
            run.error = Some(format!("{error}; recovery failed: {storage_error}"));
        }
        if let Some(observer) = &self.config.observation {
            observer.failure(
                ObservationScope {
                    session_id: run.session_id.clone(),
                    run_id: run.id.clone(),
                    ..Default::default()
                },
                "run_failed",
                0,
                &run.status,
                run.error.as_deref().unwrap_or_default(),
            );
        }
        // The same durable boundary an ordinary run error commits. A failure here
        // still reaches the waiters through the error text below.
        let log = &self.state.log;
        let settlement = log.commit(&self.store, &run.session_id, &run.id, None, |tx| {
            Store::finish_run_tx(tx, &run, None)?;
            let reference = ContentRef::Run {
                run_id: run.id.clone(),
            };
            let finished = EventKind::RunFinished {
                status: run.status.clone(),
                text: None,
                error: run.error.clone(),
                usage: None,
            };
            let events = Vec::from([
                (finished, Some(reference)),
                (EventKind::SessionSettled {}, None),
            ]);
            Ok(((), events))
        });
        if let Err(error) = settlement {
            let text = run.error.take().unwrap_or_default();
            run.error = Some(format!("{text}; settlement failed: {error}"));
        }
        if let Some(observer) = &self.config.observation {
            observer.run_finished(
                ObservationScope {
                    session_id: run.session_id.clone(),
                    run_id: run.id.clone(),
                    ..Default::default()
                },
                &run.status,
                None,
            );
        }
        let _ = self.store.runtime_idle();
        // Publish under the active lock so every waiter wakes and a later prompt
        // can be admitted. Admission cannot start another run before this point,
        // because the panicked entry still has no settled state.
        let mut active = self.state.active.lock().unwrap();
        if let Some(value) = active.get_mut(&run.session_id) {
            let mut metadata = run.clone();
            metadata.text = None;
            value.panicked = run.error.clone();
            value.finished = Some(metadata);
            value.notify.notify_waiters();
        }
        self.state.finalized.notify_all();
    }
    /// Waits for automatic queued followups too. The returned run may be the final followup.
    /// A panicked turn has no live future, so its waiters return the panic instead.
    pub async fn wait_for_idle(&self, session_id: &str) -> Result<Run> {
        loop {
            let (notify, finished, panicked) = {
                let active = self.state.active.lock().unwrap();
                let value = active
                    .get(session_id)
                    .ok_or_else(|| anyhow!("No admitted run for session"))?;
                (
                    value.notify.clone(),
                    value.finished.clone(),
                    value.panicked.clone(),
                )
            };
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(run) = finished {
                return self.settled_run(run, panicked);
            }
            let complete = {
                let active = self.state.active.lock().unwrap();
                match active.get(session_id) {
                    Some(value) => (value.finished.clone(), value.panicked.clone()),
                    None => (None, None),
                }
            };
            if let (Some(run), panicked) = complete {
                return self.settled_run(run, panicked);
            }
            notified.await;
        }
    }
    fn settled_run(&self, run: Run, panicked: Option<String>) -> Result<Run> {
        match panicked {
            Some(message) => Err(anyhow!("{message}")),
            None => self.store.hydrate_run(run),
        }
    }
}

/// A panic payload is a `&str` or a `String`; the JoinError also reports aborts.
fn panic_text(join: tokio::task::JoinError) -> String {
    if join.is_cancelled() {
        return "the run task was cancelled".into();
    }
    let payload = join.into_panic();
    if let Some(message) = payload.downcast_ref::<&str>() {
        return String::from(*message);
    }
    match payload.downcast_ref::<String>() {
        Some(message) => message.clone(),
        None => "unknown panic payload".into(),
    }
}
