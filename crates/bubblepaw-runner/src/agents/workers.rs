//! Worker acknowledgement and cleanup.
use super::*;

impl AgentTools {
    pub(super) async fn acknowledge_worker_owned(
        &self,
        owner: &str,
        address: &str,
        expected_run_id: Option<&str>,
    ) -> Result<()> {
        let released = {
            let mut state = self
                .registry
                .state
                .lock()
                .map_err(|_| anyhow!("agent registry poisoned"))?;
            let retained = state
                .children
                .get(address)
                .or_else(|| {
                    state
                        .children
                        .values()
                        .find(|child| child.session_id == address)
                })
                .cloned();
            let session_id = retained
                .as_ref()
                .map_or(address, |child| child.session_id.as_str());
            let session = match self.store.session(session_id) {
                Ok(session) => session,
                Err(_) if retained.is_none() => return Ok(()), // idempotent acknowledgement by sessionId
                Err(error) => return Err(error),
            };
            ensure!(
                session.parent_session_id.as_deref() == Some(owner),
                "worker belongs to another conversation"
            );
            let (binding, _) = self
                .store
                .attachment(&session.id, BINDING_ID)?
                .ok_or_else(|| anyhow!("session is not an internal worker"))?;
            ensure!(
                serde_json::from_str::<Binding>(&binding)?.worker,
                "session is not an internal worker"
            );
            let latest = self.store.runs(&session.id)?.into_iter().last();
            if let Some(expected) = expected_run_id
                && let Some(run) = &latest
            {
                ensure!(
                    run.id == expected,
                    "worker run does not match acknowledgement receipt"
                );
            }
            let finished = worker_finished(
                &self.store,
                &session.id,
                latest.as_ref().map(|run| run.id.as_str()),
            )?
            .ok_or_else(|| anyhow!("worker has not finished cleanup"))?;
            ensure!(
                latest.as_ref().is_none_or(|run| run.id == finished.run_id),
                "worker completion does not match latest run"
            );
            ensure!(
                expected_run_id.is_none_or(|run| run == finished.run_id),
                "worker run does not match acknowledgement receipt"
            );
            if finished.status != "idle" {
                return Ok(());
            }
            if cancelled_run(&self.store, &session.id, Some(&finished.run_id))? {
                return Ok(());
            }
            ensure!(
                self.store
                    .attachment(&session.id, &format!("{READ_PREFIX}{}", finished.run_id))?
                    .is_some(),
                "worker result has not been read"
            );
            if self.limits().worker_keep_all {
                return Ok(());
            }
            let tree = subtree_ids(&self.store.list_sessions()?, &session.id);
            ensure!(
                tree.iter().all(|id| self
                    .store
                    .pending_effects(id)
                    .is_ok_and(|effects| effects.is_empty())),
                "worker subtree has unresolved effects and must be retained"
            );
            // A successful outer task must not erase failed or undelivered nested workers.
            for id in tree.iter().filter(|id| *id != &session.id) {
                let retained = state
                    .children
                    .values()
                    .find(|child| child.session_id == *id);
                if let Some(retained) = retained {
                    let turn = retained
                        .turn
                        .lock()
                        .map_err(|_| anyhow!("agent result poisoned"))?;
                    ensure!(
                        turn.result.is_some() && !turn.cancelling,
                        "worker subtree child cleanup is still in progress"
                    );
                }
                let latest = self.store.runs(id)?.into_iter().last();
                ensure!(
                    latest.as_ref().is_some_and(|run| run.status == "completed"),
                    "worker subtree contains unfinished or unsuccessful children"
                );
                ensure!(
                    !cancelled_run(&self.store, id, latest.as_ref().map(|run| run.id.as_str()))?,
                    "worker subtree contains cancelled children"
                );
                if let Some(retained) = retained {
                    let turn = retained
                        .turn
                        .lock()
                        .map_err(|_| anyhow!("agent result poisoned"))?;
                    ensure!(
                        turn.result.as_ref().is_some_and(|reply| matches!(
                            reply["status"].as_str(),
                            Some("idle" | "completed")
                        )),
                        "worker subtree contains unsuccessful children"
                    );
                }
                if let Some(run) = &latest {
                    ensure!(
                        self.store
                            .attachment(id, &format!("{FAILURE_PREFIX}{}", run.id))?
                            .is_none(),
                        "worker subtree contains child cleanup or control failures"
                    );
                }
                let binding = self
                    .store
                    .attachment(id, BINDING_ID)?
                    .map(|(metadata, _)| serde_json::from_str::<Binding>(&metadata))
                    .transpose()?;
                let finished =
                    worker_finished(&self.store, id, latest.as_ref().map(|run| run.id.as_str()))?;
                ensure!(
                    !binding.as_ref().is_some_and(|binding| binding.worker) || finished.is_some(),
                    "worker subtree contains workers awaiting cleanup publication"
                );
                if let Some(child) = finished {
                    ensure!(
                        latest.as_ref().is_some_and(|run| run.id == child.run_id),
                        "worker subtree contains stale completion metadata"
                    );
                    ensure!(
                        child.status == "idle"
                            && self
                                .store
                                .attachment(id, &format!("{READ_PREFIX}{}", child.run_id))?
                                .is_some(),
                        "worker subtree contains unsuccessful or unread workers"
                    );
                }
            }
            self.store.delete_session_preserving_effects(&session.id)?;
            self.registry
                .approval_sources
                .lock()
                .expect("child approval sources lock")
                .retain(|session, _| !tree.contains(session));
            let released: Vec<_> = state
                .children
                .values()
                .filter(|child| tree.contains(&child.session_id))
                .cloned()
                .collect();
            state
                .children
                .retain(|_, child| !tree.contains(&child.session_id));
            released
        };
        // Canonical deletion has committed; provider mirrors are released afterward.
        for child in released {
            // Hidden children already closed their mirror before publishing a result.
            if !child.hidden {
                child
                    .harness
                    .release_deleted_session(&child.session_id)
                    .await?;
            }
        }
        self.report_worktree_cleanup().await;
        Ok(())
    }
    pub(crate) async fn report_worktree_cleanup(&self) {
        report_worktree_cleanup(&self.store, &self.registry.output).await;
    }
    /// Delete expired terminal workers, preserving every unresolved effect boundary.
    pub fn cleanup_workers(&self) -> Result<usize> {
        cleanup_workers_at(
            &self.store,
            &self.registry,
            &self.limits(),
            chrono::Utc::now(),
        )
    }
}
